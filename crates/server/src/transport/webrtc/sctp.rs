use bytes::Bytes;
use sctp_proto::{
    Association, AssociationHandle, DatagramEvent, Endpoint, EndpointConfig, Event, Payload,
    PayloadProtocolIdentifier, ServerConfig, StreamEvent,
};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DataPpi {
    Dcep,
    Binary,
    BinaryEmpty,
    String,
    StringEmpty,
    Other,
}

impl From<PayloadProtocolIdentifier> for DataPpi {
    fn from(value: PayloadProtocolIdentifier) -> Self {
        match value {
            PayloadProtocolIdentifier::Dcep => Self::Dcep,
            PayloadProtocolIdentifier::Binary => Self::Binary,
            PayloadProtocolIdentifier::BinaryEmpty => Self::BinaryEmpty,
            PayloadProtocolIdentifier::String => Self::String,
            PayloadProtocolIdentifier::StringEmpty => Self::StringEmpty,
            _ => Self::Other,
        }
    }
}

impl From<DataPpi> for PayloadProtocolIdentifier {
    fn from(value: DataPpi) -> Self {
        match value {
            DataPpi::Dcep => PayloadProtocolIdentifier::Dcep,
            DataPpi::Binary => PayloadProtocolIdentifier::Binary,
            DataPpi::BinaryEmpty => PayloadProtocolIdentifier::BinaryEmpty,
            DataPpi::String => PayloadProtocolIdentifier::String,
            DataPpi::StringEmpty => PayloadProtocolIdentifier::StringEmpty,
            DataPpi::Other => PayloadProtocolIdentifier::Binary,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SctpNotice {
    Connected,
    StreamOpened(u16),
    StreamReadable(u16),
    HandshakeFailed(String),
    AssociationLost { id: usize, reason: String },
}

pub(crate) struct PhantomSctpStack {
    endpoint: Endpoint,
    assoc_handle: Option<AssociationHandle>,
    assoc: Option<Association>,
}

impl PhantomSctpStack {
    pub(crate) fn new() -> Self {
        Self {
            endpoint: Endpoint::new(
                Arc::new(EndpointConfig::default()),
                Some(Arc::new(ServerConfig::default())),
            ),
            assoc_handle: None,
            assoc: None,
        }
    }

    pub(crate) fn handle_dtls_payload(
        &mut self,
        now: Instant,
        source: SocketAddr,
        payload: &[u8],
    ) -> Vec<SctpNotice> {
        let notices = Vec::new();
        let Some((handle, event)) = self
            .endpoint
            .handle(now, source, None, None, Bytes::copy_from_slice(payload))
        else {
            return Vec::new();
        };

        match event {
            DatagramEvent::NewAssociation(association) => {
                self.assoc_handle = Some(handle);
                self.assoc = Some(association);
            }
            DatagramEvent::AssociationEvent(event) => {
                if self.assoc_handle == Some(handle) {
                    if let Some(assoc) = self.assoc.as_mut() {
                        assoc.handle_event(event);
                    }
                }
            }
        }
        notices
    }

    pub(crate) fn poll(&mut self, now: Instant) -> Vec<SctpNotice> {
        let mut notices = Vec::new();
        while let Some(event) = self.assoc.as_mut().and_then(|assoc| assoc.poll()) {
            match event {
                Event::Connected => notices.push(SctpNotice::Connected),
                Event::Stream(StreamEvent::Opened { id, .. }) => notices.push(SctpNotice::StreamOpened(id)),
                Event::Stream(StreamEvent::Readable { id }) => notices.push(SctpNotice::StreamReadable(id)),
                Event::HandshakeFailed { reason } => {
                    notices.push(SctpNotice::HandshakeFailed(format!("{reason:?}")))
                }
                Event::AssociationLost { reason, id } => {
                    notices.push(SctpNotice::AssociationLost {
                        id: id.into(),
                        reason: format!("{reason:?}"),
                    })
                }
                _ => {}
            }
        }

        while let Some(endpoint_event) = self
            .assoc
            .as_mut()
            .and_then(|assoc| assoc.poll_endpoint_event())
        {
            if let Some(handle) = self.assoc_handle {
                let _ = self.endpoint.handle_event(handle, endpoint_event);
            }
        }

        if let Some(assoc) = self.assoc.as_mut() {
            assoc.handle_timeout(now);
        }

        notices
    }

    pub(crate) fn drain_transmits<F>(&mut self, now: Instant, mut sink: F)
    where
        F: FnMut(&[u8]),
    {
        while let Some(transmit) = self.assoc.as_mut().and_then(|assoc| assoc.poll_transmit(now)) {
            match transmit.payload {
                Payload::RawEncode(chunks) => {
                    for chunk in chunks {
                        sink(chunk.as_ref());
                    }
                }
                Payload::PartialDecode(_) => {}
            }
        }
    }

    pub(crate) fn accept_streams(&mut self) -> Vec<u16> {
        let mut ids = Vec::new();
        if let Some(assoc) = self.assoc.as_mut() {
            while let Some(stream) = assoc.accept_stream() {
                ids.push(stream.stream_identifier());
            }
        }
        ids
    }

    pub(crate) fn read_stream_messages(&mut self, stream_id: u16) -> Vec<(DataPpi, Vec<u8>)> {
        let mut messages = Vec::new();
        if let Some(assoc) = self.assoc.as_mut() {
            if let Ok(mut stream) = assoc.stream(stream_id) {
                while let Ok(Some(chunks)) = stream.read_sctp() {
                    let mut payload = vec![0u8; chunks.len()];
                    if chunks.read(&mut payload).is_ok() {
                        messages.push((chunks.ppi.into(), payload));
                    }
                }
            }
        }
        messages
    }

    pub(crate) fn write_stream(&mut self, stream_id: u16, payload: &[u8], ppi: DataPpi) {
        if let Some(assoc) = self.assoc.as_mut() {
            if let Ok(mut stream) = assoc.stream(stream_id) {
                let bytes = Bytes::copy_from_slice(payload);
                let _ = stream.write_sctp(&bytes, ppi.into());
            }
        }
    }
}
