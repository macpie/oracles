use crate::{
    traits::{MsgDecode, MsgTimestamp, TimestampDecode, TimestampEncode},
    verified_subscriber_mapping_event::VerifiedSubscriberMappingEvent,
    Error, Result,
};
use chrono::{DateTime, Utc};
use helium_proto::services::poc_mobile::VerifiedSubscriberMappingEventIngestReportV1;
use serde::{Deserialize, Serialize};

#[derive(Clone, Deserialize, Serialize, Debug, PartialEq)]
pub struct VerifiedSubscriberMappingEventIngestReport {
    pub received_timestamp: DateTime<Utc>,
    pub report: VerifiedSubscriberMappingEvent,
}

impl MsgDecode for VerifiedSubscriberMappingEventIngestReport {
    type Msg = VerifiedSubscriberMappingEventIngestReportV1;
}

impl MsgTimestamp<Result<DateTime<Utc>>> for VerifiedSubscriberMappingEventIngestReportV1 {
    fn timestamp(&self) -> Result<DateTime<Utc>> {
        self.received_timestamp.to_timestamp()
    }
}

impl MsgTimestamp<u64> for VerifiedSubscriberMappingEventIngestReport {
    fn timestamp(&self) -> u64 {
        self.received_timestamp.encode_timestamp()
    }
}

impl TryFrom<VerifiedSubscriberMappingEventIngestReportV1>
    for VerifiedSubscriberMappingEventIngestReport
{
    type Error = Error;
    fn try_from(v: VerifiedSubscriberMappingEventIngestReportV1) -> Result<Self> {
        Ok(Self {
            received_timestamp: v.timestamp()?,
            report: v
                .report
                .ok_or_else(|| Error::not_found("ingest VerifiedSubscriberMappingEvent report"))?
                .try_into()?,
        })
    }
}
