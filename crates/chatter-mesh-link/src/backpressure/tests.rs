use super::*;
use crate::link::LinkId;
use chatter_mesh::BitchatPacket;
use chatter_mesh::codec;
use chatter_mesh::packet::{MessageType, SENDER_ID_LEN};

fn link(name: &str) -> LinkId {
    LinkId::new(name)
}

fn packet(timestamp: u64) -> BitchatPacket {
    let mut sender = [0u8; SENDER_ID_LEN];
    for (i, slot) in sender.iter_mut().enumerate() {
        *slot = (i as u8) * 0x11;
    }
    BitchatPacket::new(
        MessageType::Message,
        sender,
        None,
        timestamp,
        vec![0xDE, 0xAD, 0xBE, 0xEF],
        3,
    )
}

fn frame(timestamp: u64) -> Vec<u8> {
    codec::encode(&packet(timestamp), false).expect("encode a well-formed packet")
}

mod inbound;
mod outbound;

#[test]
fn only_a_rejected_write_is_worth_buffering() {
    assert!(should_buffer(&LinkError::WriteRejected("link-1".into())));
    assert!(!should_buffer(&LinkError::LinkGone("link-1".into())));
    assert!(!should_buffer(&LinkError::PayloadTooLarge {
        actual: 600,
        max: 512
    }));
    assert!(!should_buffer(&LinkError::AdapterUnavailable("off".into())));
    assert!(!should_buffer(&LinkError::Backend("dbus".into())));
}
