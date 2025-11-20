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
        let command = match UsbIpCommand::read_from_socket(socket).await {
            Ok(c) => c,
            Err(err) => {
                if let Some(dev) = imported_device.take() {
                    server.release(dev).await;
                }
                if err.kind() == ErrorKind::UnexpectedEof {
                    info!("Remote closed the connection");
                    return Ok(());
                } else {
                    return Err(err);
                }
            }
        };
    }

        match command {
            UsbIpCommand::OpReqDevlist { .. } => {
                if let Err(e) = server.handle_op_req_devlist(socket).await {
                    error!("UsbipCommand OpReqDevlist handling error: {e:?}");
                }
            }
            UsbIpCommand::OpReqImport { busid, .. } => {
                if let Err(e) = server
                    .handle_op_req_import(socket, busid, imported_device)
                    .await
                {
                    error!("UsbipCommand OpReqImport handling error: {e:?}");
                    if let Some(dev) = imported_device.take() {
                        server.release(dev).await;
                    }
                }
                info!("Imported device: {imported_device:?}");
            }
            UsbIpCommand::UsbIpCmdSubmit {
                header,
                transfer_buffer_length,
                setup,
                data,
                ..
            } => {
                let device = match imported_device.as_ref() {
                    Some(d) => d,
                    None => {
                        error!("No device currently imported");
                        return Err(std::io::Error::other(format!("No device imported")));
                    }
                };
                if let Err(e) = server
                    .handle_usbip_cmd_submit(
                        socket,
                        header,
                        transfer_buffer_length,
                        setup,
                        data,
                        device,
                    )
                    .await
                {
                    error!("UsbipCmdSubmit handling error: {e:?}");
                }
            }
            UsbIpCommand::UsbIpCmdUnlink {
                header,
                unlink_seqnum,
            } => {
                if let Err(e) = server
                    .handle_usbip_cmd_unlink(socket, header, unlink_seqnum)
                    .await
                {
                    error!("UsbipCmdUnlink handling error: {e:?}");
                }
            }
        }
}