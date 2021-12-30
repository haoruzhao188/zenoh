//
// Copyright (c) 2017, 2020 ADLINK Technology Inc.
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ADLINK zenoh team, <zenoh@adlink-labs.tech>
//
mod init_ack;
mod init_syn;
mod open_ack;
mod open_syn;

use super::authenticator::AuthenticatedPeerLink;
use super::*;
use crate::net::link::LinkUnicast;
use crate::net::protocol::proto::tmsg;
use crate::net::transport::TransportManager;
use zenoh_util::core::Result as ZResult;

pub(super) type AError = (zenoh_util::core::Error, Option<u8>);
pub(super) type AResult<T> = Result<T, AError>;

pub(crate) async fn accept_link(
    link: &LinkUnicast,
    manager: &TransportManager,
    auth_link: &mut AuthenticatedPeerLink,
) -> ZResult<()> {
    // INIT handshake
    macro_rules! step {
        ($s: expr) => {
            match $s {
                Ok(output) => output,
                Err((e, reason)) => {
                    close_link(link, manager, auth_link, reason).await;
                    return Err(e);
                }
            }
        };
    }

    let output = step!(init_syn::recv(link, manager, auth_link).await);
    let output = step!(init_ack::send(link, manager, auth_link, output).await);
    let output = step!(open_syn::recv(link, manager, auth_link, output).await);

    // Initialize the transport
    macro_rules! step {
        ($s: expr) => {
            match $s {
                Ok(output) => output,
                Err(e) => {
                    close_link(link, manager, auth_link, Some(tmsg::close_reason::INVALID)).await;
                    return Err(e);
                }
            }
        };
    }

    let pid = output.cookie.pid;
    let input = super::InputInit {
        pid: output.cookie.pid,
        whatami: output.cookie.whatami,
        sn_resolution: output.cookie.sn_resolution,
        is_shm: output.is_shm,
        is_qos: output.cookie.is_qos,
    };
    let transport = step!(self::transport_init(manager, input).await);

    // OPEN handshake
    macro_rules! step {
        ($s: expr) => {
            match $s {
                Ok(output) => output,
                Err((e, reason)) => {
                    if let Ok(ll) = transport.get_links() {
                        if ll.is_empty() {
                            let _ = manager.del_transport_unicast(&pid).await;
                        }
                    }
                    close_link(link, manager, auth_link, reason).await;
                    return Err(e);
                }
            }
        };
    }

    // Add the link to the transport
    let _ = step!(step!(transport
        .get_inner()
        .map_err(|e| (e, Some(tmsg::close_reason::INVALID))))
    .add_link(link.clone())
    .map_err(|e| (e, Some(tmsg::close_reason::MAX_LINKS))));

    // Sync the RX sequence number
    let _ = step!(transport
        .get_inner()
        .map_err(|e| (e, Some(tmsg::close_reason::INVALID))))
    .sync(output.initial_sn)
    .await;

    log::debug!("New transport link established from {}: {}", pid, link);

    let initial_sn = step!(transport
        .get_inner()
        .map_err(|e| (e, Some(tmsg::close_reason::INVALID))))
    .config
    .initial_sn_tx;
    let input = open_ack::Input {
        initial_sn,
        attachment: output.open_ack_attachment,
    };
    let lease = output.lease;
    let _output = step!(open_ack::send(link, manager, auth_link, input).await);

    let input = self::InputFinalize {
        transport: transport.clone(),
        lease,
    };
    let _ = step!(transport_finalize(link, manager, input)
        .await
        .map_err(|e| (e, Some(tmsg::close_reason::INVALID))));

    Ok(())
}