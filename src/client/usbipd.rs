use super::usbip_protocol::{UsbIpCommand, UsbIpResponse, UsbSpeed};
use super::usbip_protocol::{EP0_MAX_PACKET_SIZE, USBIP_RET_SUBMIT, USBIP_RET_UNLINK};
use hbb_common::tokio_util::io::StreamReader;
use hbb_common::{
    allow_err,
    futures::stream::iter,
    message_proto::{self, Message},
    tokio::{io, sync::RwLock},
};
use hbb_common::{log, Stream};
use num_derive::FromPrimitive;
use num_traits::FromPrimitive;
use nusbip::UsbIpServer;
use nusbip::usbip_protocol::UsbIpCommand;
use std::any::Any;
use std::collections::HashMap;
use std::io::{ErrorKind,Read, Write};
use std::sync::{Arc, Mutex};
use nusb::{
    MaybeFuture,
    transfer::{Bulk, In, Interrupt, Out},
};

pub async fn read_usbip_command_from_bytes(raw: bytes::Bytes) -> io::Result<UsbIpCommand> {
    let stream = iter(vec![io::Result::Ok(raw)]);
    let mut reader = StreamReader::new(stream);
    UsbIpCommand::read_from_socket(&mut reader).await
}

pub async fn handler(
    server: &UsbIpServer,
    command: io::Result<UsbIpCommand>,
    current_import_device_id: Arc<Mutex<Option<String>>>,
    peer: &mut Stream,
) -> std::io::Result<()> {
    if let Err(err) = command {
        let current_import_device_id = current_import_device_id.lock().unwrap();
        if let Some(dev_id) = current_import_device_id.as_ref() {
            let mut used_devices = server.used_devices.write().await;
            let mut available_devices = server.available_devices.write().await;
            match used_devices.remove(dev_id) {
                Some(dev) => available_devices.push(dev),
                None => unreachable!(),
            }
        }

        if err.kind() == ErrorKind::UnexpectedEof {
            log::info!("Remote closed the connection");
            return Ok(());
        } else {
            return Err(err);
        }
    }

    let used_devices = server.used_devices.read().await;
    let mut current_import_device = {
        let cidi = current_import_device_id.lock().unwrap();
        cidi.clone().and_then(|ref id| used_devices.get(id))
    };

    let command = command?;
    match command {
        UsbIpCommand::OpReqDevlist { .. } => {
            log::info!("Got OP_REQ_DEVLIST");
            let devices = server.available_devices.read().await;

            // OP_REP_DEVLIST
            let res = UsbIpResponse::op_rep_devlist(devices.as_slice());
            let mut msg = Message::new();
            msg.set_usb_ip_response(message_proto::UsbIpResponse {
                raw_response: res.to_bytes().into(),
                ..Default::default()
            });
            allow_err!(peer.send(&msg).await);
            // UsbIpResponse::op_rep_devlist(&devices)
            //     .write_to_socket(socket)
            //     .await?;
            log::info!("Sent OP_REP_DEVLIST");
        }
        UsbIpCommand::OpReqImport { busid, .. } => {
            log::info!("Got OP_REQ_IMPORT");
            {
                let mut cidi = current_import_device_id.lock().unwrap();
                *cidi = None;
            }
            current_import_device = None;
            std::mem::drop(used_devices);

            let mut used_devices = server.used_devices.write().await;
            let mut available_devices = server.available_devices.write().await;
            let busid_compare =
                &busid[..busid.iter().position(|&x| x == 0).unwrap_or(busid.len())];
            for (i, dev) in available_devices.iter().enumerate() {
                if busid_compare == dev.bus_id.as_bytes() {
                    let dev = available_devices.remove(i);
                    let dev_id = dev.bus_id.clone();
                    used_devices.insert(dev.bus_id.clone(), dev);
                    {
                        let mut cidi = current_import_device_id.lock().unwrap();
                        *cidi = Some(dev_id.clone());
                    }
                    // current_import_device_id = dev_id.clone().into();
                    current_import_device = Some(used_devices.get(&dev_id).unwrap());
                    break;
                }
            }

            let res = if let Some(dev) = current_import_device {
                UsbIpResponse::op_rep_import_success(dev)
            } else {
                UsbIpResponse::op_rep_import_fail()
            };

            let mut msg = Message::new();
            msg.set_usb_ip_response(message_proto::UsbIpResponse {
                raw_response: res.to_bytes().into(),
                ..Default::default()
            });
            allow_err!(peer.send(&msg).await);
            // res.write_to_socket(socket).await?;
            log::info!("Sent OP_REP_IMPORT");
        }
        UsbIpCommand::UsbIpCmdSubmit {
            mut header,
            transfer_buffer_length,
            setup,
            data,
            ..
        } => {
            log::info!("Got USBIP_CMD_SUBMIT");
            let device = current_import_device.unwrap();

            let out = header.direction == 0;
            let real_ep = if out { header.ep } else { header.ep | 0x80 };

            header.command = USBIP_RET_SUBMIT.into();

            let res = match device.find_ep(real_ep as u8) {
                None => {
                    log::warn!("Endpoint {real_ep:02x?} not found");
                    UsbIpResponse::usbip_ret_submit_fail(&header)
                }
                Some((ep, intf)) => {
                    log::info!("->Endpoint {ep:02x?}");
                    log::info!("->Setup {setup:02x?}");
                    log::info!("->Request {data:02x?}");
                    let resp = device
                        .handle_urb(
                            ep,
                            intf,
                            transfer_buffer_length,
                            SetupPacket::parse(&setup),
                            &data,
                        )
                        .await;

                    match resp {
                        Ok(resp) => {
                            if out {
                                log::info!("<-Wrote {}", data.len());
                            } else {
                                log::info!("<-Resp {resp:02x?}");
                            }
                            UsbIpResponse::usbip_ret_submit_success(&header, 0, 0, resp, vec![])
                        }
                        Err(err) => {
                            log::warn!("Error handling URB: {err}");
                            UsbIpResponse::usbip_ret_submit_fail(&header)
                        }
                    }
                }
            };

            let mut msg = Message::new();
            msg.set_usb_ip_response(message_proto::UsbIpResponse {
                raw_response: res.to_bytes().into(),
                ..Default::default()
            });
            allow_err!(peer.send(&msg).await);
            // res.write_to_socket(socket).await?;
            log::info!("Sent USBIP_RET_SUBMIT");
        }
        UsbIpCommand::UsbIpCmdUnlink {
            mut header,
            unlink_seqnum,
        } => {
            log::info!("Got USBIP_CMD_UNLINK for {unlink_seqnum:10x?}");

            header.command = USBIP_RET_UNLINK.into();

            let res = UsbIpResponse::usbip_ret_unlink_success(&header);
            let mut msg = Message::new();
            msg.set_usb_ip_response(message_proto::UsbIpResponse {
                raw_response: res.to_bytes().into(),
                ..Default::default()
            });
            allow_err!(peer.send(&msg).await);
            // res.write_to_socket(socket).await?;
            log::info!("Sent USBIP_RET_UNLINK");
        }
    }
    Ok(())
}