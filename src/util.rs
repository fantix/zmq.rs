use crate::codec::{CodecResult, FramedIo};
use crate::*;

use bytes::Bytes;
use futures::stream::StreamExt;
use futures::SinkExt;
use futures_codec::FramedRead;
use std::convert::{TryFrom, TryInto};
use std::sync::Arc;
use uuid::Uuid;

#[derive(PartialEq, Eq, PartialOrd, Ord, Debug, Hash, Clone)]
pub struct PeerIdentity(Vec<u8>);

impl PeerIdentity {
    pub fn new() -> Self {
        let id = Uuid::new_v4();
        Self(id.as_bytes().to_vec())
    }
}

impl Default for PeerIdentity {
    fn default() -> Self {
        Self::new()
    }
}

impl TryFrom<Vec<u8>> for PeerIdentity {
    type Error = ZmqError;

    fn try_from(data: Vec<u8>) -> Result<Self, ZmqError> {
        if data.is_empty() {
            Ok(PeerIdentity::new())
        } else if data.len() > 255 {
            Err(ZmqError::Other(
                "ZMQ_IDENTITY should not be more than 255 bytes long",
            ))
        } else {
            Ok(Self(data))
        }
    }
}

impl From<PeerIdentity> for Vec<u8> {
    fn from(p_id: PeerIdentity) -> Self {
        p_id.0
    }
}

impl From<PeerIdentity> for Bytes {
    fn from(p_id: PeerIdentity) -> Self {
        Bytes::from(p_id.0)
    }
}

pub(crate) struct Peer {
    pub(crate) _identity: PeerIdentity,
    pub(crate) send_queue: FramedWrite<Box<dyn FrameableWrite>, ZmqCodec>,
    pub(crate) recv_queue: FramedRead<Box<dyn FrameableRead>, ZmqCodec>,
}

const COMPATIBILITY_MATRIX: [u8; 121] = [
    // PAIR, PUB, SUB, REQ, REP, DEALER, ROUTER, PULL, PUSH, XPUB, XSUB
    1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // PAIR
    0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1, // PUB
    0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0, // SUB
    0, 0, 0, 0, 1, 0, 1, 0, 0, 0, 0, // REQ
    0, 0, 0, 1, 0, 1, 0, 0, 0, 0, 0, // REP
    0, 0, 0, 0, 1, 1, 1, 0, 0, 0, 0, // DEALER
    0, 0, 0, 1, 0, 1, 1, 0, 0, 0, 0, // ROUTER
    0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, // PULL
    0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, // PUSH
    0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1, // XPUB
    0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0, // XSUB
];

/// Checks if two sokets are compatible with each other
/// ```
/// use zeromq::util::sockets_compatible;
/// use zeromq::SocketType;
/// assert!(sockets_compatible(SocketType::PUB, SocketType::SUB));
/// assert!(sockets_compatible(SocketType::REQ, SocketType::REP));
/// assert!(sockets_compatible(SocketType::DEALER, SocketType::ROUTER));
/// assert!(!sockets_compatible(SocketType::PUB, SocketType::REP));
/// ```
pub fn sockets_compatible(one: SocketType, another: SocketType) -> bool {
    let row_index = one.to_usize().unwrap();
    let col_index = another.to_usize().unwrap();
    COMPATIBILITY_MATRIX[row_index * 11 + col_index] != 0
}

/// Given the result of the greetings exchange, determines the version of the
/// ZMTP protocol that should be used for communication with the peer according
/// to https://rfc.zeromq.org/spec/23/#version-negotiation.
fn negotiate_version(greeting: Option<CodecResult<Message>>) -> ZmqResult<ZmtpVersion> {
    let my_version = ZmqGreeting::default().version;

    match greeting {
        Some(Ok(Message::Greeting(peer))) => {
            if peer.version >= my_version {
                // A peer MUST accept higher protocol versions as valid. That is,
                // a ZMTP peer MUST accept protocol versions greater or equal to 3.0.
                // This allows future implementations to safely interoperate with
                // current implementations.
                //
                // A peer SHALL always use its own protocol (including framing)
                // when talking to an equal or higher protocol peer.
                Ok(my_version)
            } else {
                // A peer MAY downgrade its protocol to talk to a lower protocol peer.
                //
                // If a peer cannot downgrade its protocol to match its peer, it MUST
                // close the connection.
                // TODO: implement interoperability with older protocol versions
                Err(ZmqError::UnsupportedVersion(peer.version))
            }
        }
        _ => Err(ZmqError::Other("Failed Greeting exchange")),
    }
}

pub(crate) async fn greet_exchange(raw_socket: &mut FramedIo) -> ZmqResult<ZmtpVersion> {
    raw_socket
        .write_half
        .send(Message::Greeting(ZmqGreeting::default()))
        .await?;

    let greeting: Option<CodecResult<Message>> = raw_socket.read_half.next().await;
    negotiate_version(greeting)
}

pub(crate) async fn ready_exchange(
    raw_socket: &mut FramedIo,
    socket_type: SocketType,
) -> ZmqResult<PeerIdentity> {
    let ready = ZmqCommand::ready(socket_type);
    raw_socket.write_half.send(Message::Command(ready)).await?;

    let ready_repl: Option<CodecResult<Message>> = raw_socket.read_half.next().await;
    match ready_repl {
        Some(Ok(Message::Command(command))) => match command.name {
            ZmqCommandName::READY => {
                let other_sock_type = command
                    .properties
                    .get("Socket-Type")
                    .map(|x| SocketType::try_from(x.as_str()))
                    .unwrap_or(Err(ZmqError::Other("Failed to parse other socket type")))?;

                let peer_id = command
                    .properties
                    .get("Identity")
                    .map_or_else(PeerIdentity::new, |x| {
                        x.clone().into_bytes().try_into().unwrap()
                    });

                if sockets_compatible(socket_type, other_sock_type) {
                    Ok(peer_id)
                } else {
                    Err(ZmqError::Other(
                        "Provided sockets combination is not compatible",
                    ))
                }
            }
        },
        Some(Ok(_)) => Err(ZmqError::Other("Failed to confirm ready state")),
        Some(Err(e)) => Err(e.into()),
        None => Err(ZmqError::Other("No reply from server")),
    }
}

pub(crate) async fn peer_connected(
    mut raw_socket: FramedIo,
    backend: Arc<dyn MultiPeerBackend>,
) -> ZmqResult<PeerIdentity> {
    greet_exchange(&mut raw_socket).await?;
    let peer_id = ready_exchange(&mut raw_socket, backend.socket_type()).await?;
    backend.peer_connected(&peer_id, raw_socket);
    Ok(peer_id)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::codec::mechanism::ZmqMechanism;

    pub async fn test_bind_to_unspecified_interface_helper(
        any: std::net::IpAddr,
        mut sock: impl Socket,
        start_port: u16,
    ) -> ZmqResult<()> {
        assert!(sock.binds().is_empty());
        assert!(any.is_unspecified());

        for i in 0..4 {
            sock.bind(
                Endpoint::Tcp(any.into(), start_port + i)
                    .to_string()
                    .as_str(),
            )
            .await?;
        }

        let bound_to = sock.binds();
        assert_eq!(bound_to.len(), 4);

        let mut port_set = std::collections::HashSet::new();
        for b in bound_to.keys() {
            if let Endpoint::Tcp(host, port) = b {
                assert_eq!(host, &any.into());
                port_set.insert(*port);
            } else {
                unreachable!()
            }
        }

        (start_port..start_port + 4).for_each(|p| assert!(port_set.contains(&p)));

        Ok(())
    }

    pub async fn test_bind_to_any_port_helper(mut sock: impl Socket) -> ZmqResult<()> {
        assert!(sock.binds().is_empty());
        for _ in 0..4 {
            sock.bind("tcp://localhost:0").await?;
        }

        let bound_to = sock.binds();
        assert_eq!(bound_to.len(), 4);
        let mut port_set = std::collections::HashSet::new();
        for b in bound_to.keys() {
            if let Endpoint::Tcp(host, port) = b {
                assert_eq!(host, &Host::Domain("localhost".to_string()));
                assert_ne!(*port, 0);
                // Insert and check that it wasn't already present
                assert!(port_set.insert(*port));
            } else {
                unreachable!()
            }
        }

        Ok(())
    }

    fn new_greeting(version: ZmtpVersion) -> CodecResult<Message> {
        Ok(Message::Greeting(ZmqGreeting {
            version,
            mechanism: ZmqMechanism::PLAIN,
            as_server: false,
        }))
    }

    #[test]
    fn negotiate_version_peer_is_using_the_same_version() {
        // if both peers are using the same protocol version, negotiation is trivial
        let peer_version = ZmqGreeting::default().version;
        let expected = ZmqGreeting::default().version;
        let actual = negotiate_version(Some(new_greeting(peer_version))).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn negotiate_version_peer_is_using_a_newer_version() {
        // if the other end is using a newer protocol version, they should adjust to us
        let peer_version = (3, 1);
        let expected = ZmqGreeting::default().version;
        let actual = negotiate_version(Some(new_greeting(peer_version))).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn negotiate_version_peer_is_using_an_older_version() {
        // if the other end is using an older protocol version, we should adjust to
        // them, but interoperability with older peers is not implemented at the
        // moment, so we just give up immediately, which is allowed by the spec
        let peer_version = (2, 1);
        let actual = negotiate_version(Some(new_greeting(peer_version)));
        match actual {
            Err(ZmqError::UnsupportedVersion(version)) => assert_eq!(version, peer_version),
            _ => panic!("Unexpected result"),
        }
    }

    #[test]
    fn negotiate_version_invalid_greeting() {
        // could not read the greeting message
        let actual = negotiate_version(None);
        match actual {
            Err(ZmqError::Other(_)) => {}
            _ => panic!("Unexpected result"),
        }

        // unexpected message during greetings exchange
        let message = Ok(Message::Message(ZmqMessage::from("")));
        let actual = negotiate_version(Some(message));
        match actual {
            Err(ZmqError::Other(_)) => {}
            _ => panic!("Unexpected result"),
        }
    }
}
