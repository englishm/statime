use core::fmt::Debug;

use atomic_refcell::AtomicRefCell;

use crate::{
    clock::Clock,
    datastructures::{
        common::PortIdentity,
        datasets::DefaultDS,
        messages::{DelayReqMessage, Message},
    },
    port::{
        sequence_id::SequenceIdGenerator, PortAction, PortActionIterator, TimestampContext,
        TimestampContextInner,
    },
    ptp_instance::PtpInstanceState,
    time::{Interval, Time},
    PortConfig,
};

#[derive(Clone, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub(crate) struct MasterState {
    pub(in crate::port) announce_seq_ids: SequenceIdGenerator,
    pub(in crate::port) sync_seq_ids: SequenceIdGenerator,
}

impl MasterState {
    pub(crate) fn new() -> Self {
        MasterState {
            announce_seq_ids: SequenceIdGenerator::new(),
            sync_seq_ids: SequenceIdGenerator::new(),
        }
    }

    pub(crate) fn handle_timestamp<'a>(
        &mut self,
        context: TimestampContext,
        timestamp: Time,
        port_identity: PortIdentity,
        default_ds: &DefaultDS,
        buffer: &'a mut [u8],
    ) -> PortActionIterator<'a> {
        match context.inner {
            TimestampContextInner::Sync { id } => {
                self.handle_sync_timestamp(id, timestamp, port_identity, default_ds, buffer)
            }
            _ => {
                log::error!("Unexpected send timestamp");
                actions![]
            }
        }
    }

    pub(crate) fn handle_sync_timestamp<'a>(
        &mut self,
        id: u16,
        timestamp: Time,
        port_identity: PortIdentity,
        default_ds: &DefaultDS,
        buffer: &'a mut [u8],
    ) -> PortActionIterator<'a> {
        let packet_length =
            match Message::follow_up(default_ds, port_identity, id, timestamp).serialize(buffer) {
                Ok(length) => length,
                Err(error) => {
                    log::error!(
                        "Statime bug: Could not serialize sync follow up {:?}",
                        error
                    );
                    return actions![];
                }
            };

        actions![PortAction::SendGeneral {
            data: &buffer[..packet_length],
        }]
    }

    pub(crate) fn send_sync<'a>(
        &mut self,
        local_clock: &AtomicRefCell<impl Clock>,
        config: &PortConfig,
        port_identity: PortIdentity,
        default_ds: &DefaultDS,
        buffer: &'a mut [u8],
    ) -> PortActionIterator<'a> {
        log::trace!("sending sync message");

        let current_time = match local_clock.try_borrow().map(|borrow| borrow.now()) {
            Ok(time) => time,
            Err(error) => {
                log::error!("Statime bug: Clock busy {:?}", error);
                return actions![];
            }
        };

        let seq_id = self.sync_seq_ids.generate();
        let packet_length = match Message::sync(default_ds, port_identity, seq_id, current_time)
            .serialize(buffer)
        {
            Ok(message) => message,
            Err(error) => {
                log::error!("Statime bug: Could not serialize sync: {:?}", error);
                return actions![];
            }
        };

        actions![
            PortAction::ResetSyncTimer {
                duration: config.sync_interval.as_core_duration(),
            },
            PortAction::SendTimeCritical {
                context: TimestampContext {
                    inner: TimestampContextInner::Sync { id: seq_id },
                },
                data: &buffer[..packet_length],
            }
        ]
    }

    pub(crate) fn send_announce<'a, C: Clock, F>(
        &mut self,
        global: &PtpInstanceState<C, F>,
        config: &PortConfig,
        port_identity: PortIdentity,
        buffer: &'a mut [u8],
    ) -> PortActionIterator<'a> {
        log::trace!("sending announce message");

        let current_time = match global.local_clock.try_borrow().map(|borrow| borrow.now()) {
            Ok(time) => time,
            Err(error) => {
                log::error!("Statime bug: clock busy {:?}", error);
                return actions![];
            }
        };

        let packet_length = match Message::announce(
            global,
            port_identity,
            self.announce_seq_ids.generate(),
            current_time,
        )
        .serialize(buffer)
        {
            Ok(length) => length,
            Err(error) => {
                log::error!(
                    "Statime bug: Could not serialize announce message {:?}",
                    error
                );
                return actions![];
            }
        };

        actions![
            PortAction::ResetAnnounceTimer {
                duration: config.announce_interval.as_core_duration(),
            },
            PortAction::SendGeneral {
                data: &buffer[..packet_length]
            }
        ]
    }

    pub(crate) fn handle_event_receive<'a>(
        &mut self,
        message: Message,
        timestamp: Time,
        min_delay_req_interval: Interval,
        port_identity: PortIdentity,
        buffer: &'a mut [u8],
    ) -> PortActionIterator<'a> {
        if message.header().source_port_identity == port_identity {
            return actions![];
        }

        match message {
            Message::DelayReq(message) => self.handle_delay_req(
                message,
                timestamp,
                min_delay_req_interval,
                port_identity,
                buffer,
            ),
            _ => {
                log::warn!("Unexpected message {:?}", message);
                actions![]
            }
        }
    }

    fn handle_delay_req<'a>(
        &mut self,
        message: DelayReqMessage,
        timestamp: Time,
        min_delay_req_interval: Interval,
        port_identity: PortIdentity,
        buffer: &'a mut [u8],
    ) -> PortActionIterator<'a> {
        log::debug!("Received DelayReq");
        let delay_resp_message =
            Message::delay_resp(&message, port_identity, min_delay_req_interval, timestamp);

        let packet_length = match delay_resp_message.serialize(buffer) {
            Ok(length) => length,
            Err(error) => {
                log::error!("Could not serialize delay response: {:?}", error);
                return actions![];
            }
        };

        actions![PortAction::SendGeneral {
            data: &buffer[..packet_length],
        }]
    }
}

#[cfg(test)]
mod tests {
    use fixed::types::{I48F16, U96F32};

    use super::*;
    use crate::{
        config::InstanceConfig,
        datastructures::{
            common::{ClockIdentity, TimeInterval},
            datasets::{CurrentDS, ParentDS},
            messages::{Header, SdoId},
        },
        time::Interval,
        Duration, TimePropertiesDS, MAX_DATA_LEN,
    };

    struct TestClock {
        current_time: Time,
    }

    impl Clock for TestClock {
        type Error = std::convert::Infallible;

        fn now(&self) -> Time {
            self.current_time
        }

        fn adjust(
            &mut self,
            _time_offset: crate::time::Duration,
            _frequency_multiplier: f64,
            _time_properties_ds: &crate::datastructures::datasets::TimePropertiesDS,
        ) -> core::result::Result<(), Self::Error> {
            panic!("Shouldn't be called");
        }
    }

    #[test]
    fn test_delay_response() {
        let mut state = MasterState::new();

        let mut buffer = [0u8; MAX_DATA_LEN];

        let mut action = state.handle_event_receive(
            Message::DelayReq(DelayReqMessage {
                header: Header {
                    sequence_id: 5123,
                    source_port_identity: PortIdentity {
                        port_number: 83,
                        ..Default::default()
                    },
                    correction_field: TimeInterval(I48F16::from_bits(400)),
                    ..Default::default()
                },
                origin_timestamp: Time::from_micros(0).into(),
            }),
            Time::from_fixed_nanos(U96F32::from_bits((200000 << 32) + (500 << 16))),
            Interval::from_log_2(2),
            PortIdentity::default(),
            &mut buffer,
        );

        let Some(PortAction::SendGeneral { data }) = action.next() else {
            panic!("Unexpected resulting action");
        };
        assert!(action.next().is_none());
        drop(action);

        let msg = match Message::deserialize(data).unwrap() {
            Message::DelayResp(msg) => msg,
            _ => panic!("Unexpected message type"),
        };

        assert_eq!(
            msg.requesting_port_identity,
            PortIdentity {
                port_number: 83,
                ..Default::default()
            }
        );
        assert_eq!(msg.header.sequence_id, 5123);
        assert_eq!(msg.receive_timestamp, Time::from_micros(200).into());
        assert_eq!(msg.header.log_message_interval, 2);
        assert_eq!(
            msg.header.correction_field,
            TimeInterval(I48F16::from_bits(900))
        );

        let mut action = state.handle_event_receive(
            Message::DelayReq(DelayReqMessage {
                header: Header {
                    sequence_id: 879,
                    source_port_identity: PortIdentity {
                        port_number: 12,
                        ..Default::default()
                    },
                    correction_field: TimeInterval(I48F16::from_bits(200)),
                    ..Default::default()
                },
                origin_timestamp: Time::from_micros(0).into(),
            }),
            Time::from_fixed_nanos(U96F32::from_bits((220000 << 32) + (300 << 16))),
            Interval::from_log_2(5),
            PortIdentity::default(),
            &mut buffer,
        );

        let Some(PortAction::SendGeneral { data }) = action.next() else {
            panic!("Unexpected resulting action");
        };
        assert!(action.next().is_none());

        let msg = match Message::deserialize(data).unwrap() {
            Message::DelayResp(msg) => msg,
            _ => panic!("Unexpected message type"),
        };

        assert_eq!(
            msg.requesting_port_identity,
            PortIdentity {
                port_number: 12,
                ..Default::default()
            }
        );
        assert_eq!(msg.header.sequence_id, 879);
        assert_eq!(msg.receive_timestamp, Time::from_micros(220).into());
        assert_eq!(msg.header.log_message_interval, 5);
        assert_eq!(
            msg.header.correction_field,
            TimeInterval(I48F16::from_bits(500))
        );
    }

    #[test]
    fn test_announce() {
        let mut buffer = [0u8; MAX_DATA_LEN];

        let default_ds = DefaultDS::new(InstanceConfig {
            clock_identity: ClockIdentity::default(),
            priority_1: 15,
            priority_2: 128,
            domain_number: 0,
            slave_only: false,
            sdo_id: SdoId::default(),
        });
        let mut parent_ds = ParentDS::new(default_ds);
        parent_ds.grandmaster_priority_1 = 15;
        let current_ds = CurrentDS::default();
        let time_properties_ds = TimePropertiesDS::default();
        let global = PtpInstanceState {
            default_ds,
            current_ds,
            parent_ds,
            time_properties_ds,
            local_clock: AtomicRefCell::new(TestClock {
                current_time: Time::from_micros(600),
            }),
            filter: AtomicRefCell::new(()),
        };

        let config = PortConfig {
            delay_mechanism: crate::DelayMechanism::E2E {
                interval: Interval::TWO_SECONDS,
            },
            announce_interval: Interval::TWO_SECONDS,
            announce_receipt_timeout: 2,
            sync_interval: Interval::ONE_SECOND,
            master_only: false,
            delay_asymmetry: Duration::ZERO,
        };
        let mut state = MasterState::new();

        let mut actions =
            state.send_announce(&global, &config, PortIdentity::default(), &mut buffer);

        assert!(matches!(
            actions.next(),
            Some(PortAction::ResetAnnounceTimer { .. })
        ));
        let Some(PortAction::SendGeneral { data }) = actions.next() else {
            panic!("Unexpected action");
        };
        assert!(actions.next().is_none());
        drop(actions);

        let msg = match Message::deserialize(data).unwrap() {
            Message::Announce(msg) => msg,
            _ => panic!("Unexpected message type"),
        };

        assert_eq!(msg.grandmaster_priority_1, 15);

        let mut actions =
            state.send_announce(&global, &config, PortIdentity::default(), &mut buffer);

        assert!(matches!(
            actions.next(),
            Some(PortAction::ResetAnnounceTimer { .. })
        ));
        let Some(PortAction::SendGeneral { data }) = actions.next() else {
            panic!("Unexpected action");
        };
        assert!(actions.next().is_none());

        let msg2 = match Message::deserialize(data).unwrap() {
            Message::Announce(msg) => msg,
            _ => panic!("Unexpected message type"),
        };

        assert_eq!(msg2.grandmaster_priority_1, 15);
        assert_ne!(msg2.header.sequence_id, msg.header.sequence_id);
    }

    #[test]
    fn test_sync() {
        let mut buffer = [0u8; MAX_DATA_LEN];
        let config = PortConfig {
            delay_mechanism: crate::DelayMechanism::E2E {
                interval: Interval::TWO_SECONDS,
            },
            announce_interval: Interval::TWO_SECONDS,
            announce_receipt_timeout: 2,
            sync_interval: Interval::ONE_SECOND,
            master_only: false,
            delay_asymmetry: crate::Duration::ZERO,
        };

        let clock = AtomicRefCell::new(TestClock {
            current_time: Time::from_fixed_nanos(U96F32::from_bits((600000 << 32) + (248 << 16))),
        });

        let mut state = MasterState::new();
        let defaultds = DefaultDS::new(InstanceConfig {
            clock_identity: ClockIdentity::default(),
            priority_1: 15,
            priority_2: 128,
            domain_number: 0,
            slave_only: false,
            sdo_id: SdoId::default(),
        });

        let mut actions = state.send_sync(
            &clock,
            &config,
            PortIdentity::default(),
            &defaultds,
            &mut buffer,
        );

        assert!(matches!(
            actions.next(),
            Some(PortAction::ResetSyncTimer { .. })
        ));
        let Some(PortAction::SendTimeCritical { context, data }) = actions.next() else {
            panic!("Unexpected action");
        };
        assert!(actions.next().is_none());
        drop(actions);

        let sync = match Message::deserialize(&data).unwrap() {
            Message::Sync(msg) => msg,
            _ => panic!("Unexpected message type"),
        };

        let mut actions = state.handle_timestamp(
            context,
            Time::from_fixed_nanos(U96F32::from_bits((601300 << 32) + (230 << 16))),
            PortIdentity::default(),
            &defaultds,
            &mut buffer,
        );

        let Some(PortAction::SendGeneral { data }) = actions.next() else {
            panic!("Unexpected action");
        };
        assert!(actions.next().is_none());
        drop(actions);

        let follow = match Message::deserialize(&data).unwrap() {
            Message::FollowUp(msg) => msg,
            _ => panic!("Unexpected message type"),
        };

        assert_eq!(sync.header.sequence_id, follow.header.sequence_id);
        assert_eq!(sync.origin_timestamp, Time::from_micros(600).into());
        assert_eq!(
            sync.header.correction_field,
            TimeInterval(I48F16::from_bits(0))
        );
        assert_eq!(
            follow.precise_origin_timestamp,
            Time::from_fixed_nanos(601300).into()
        );
        assert_eq!(
            follow.header.correction_field,
            TimeInterval(I48F16::from_bits(230))
        );

        clock.borrow_mut().current_time =
            Time::from_fixed_nanos(U96F32::from_bits((1000600000 << 32) + (192 << 16)));
        let mut actions = state.send_sync(
            &clock,
            &config,
            PortIdentity::default(),
            &defaultds,
            &mut buffer,
        );

        assert!(matches!(
            actions.next(),
            Some(PortAction::ResetSyncTimer { .. })
        ));
        let Some(PortAction::SendTimeCritical { context, data }) = actions.next() else {
            panic!("Unexpected action");
        };
        assert!(actions.next().is_none());
        drop(actions);

        let sync2 = match Message::deserialize(&data).unwrap() {
            Message::Sync(msg) => msg,
            _ => panic!("Unexpected message type"),
        };

        let mut actions = state.handle_timestamp(
            context,
            Time::from_fixed_nanos(U96F32::from_bits((1000601300 << 32) + (543 << 16))),
            PortIdentity::default(),
            &defaultds,
            &mut buffer,
        );

        let Some(PortAction::SendGeneral { data }) = actions.next() else {
            panic!("Unexpected action");
        };
        assert!(actions.next().is_none());

        let follow2 = match Message::deserialize(&data).unwrap() {
            Message::FollowUp(msg) => msg,
            _ => panic!("Unexpected message type"),
        };

        assert_ne!(sync.header.sequence_id, sync2.header.sequence_id);
        assert_eq!(sync2.header.sequence_id, follow2.header.sequence_id);
        assert_eq!(sync2.origin_timestamp, Time::from_micros(1000600).into());
        assert_eq!(
            sync2.header.correction_field,
            TimeInterval(I48F16::from_bits(0))
        );
        assert_eq!(
            follow2.precise_origin_timestamp,
            Time::from_fixed_nanos(1000601300).into()
        );
        assert_eq!(
            follow2.header.correction_field,
            TimeInterval(I48F16::from_bits(543))
        );
    }
}
