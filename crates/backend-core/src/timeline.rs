use thiserror::Error;

use crate::types::{TimelineItem, TimelineOp};

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum TimelineMergeError {
    #[error("timeline item with event_id '{0}' was not found")]
    MissingEvent(String),
}

#[derive(Debug, Clone)]
pub struct TimelineBuffer {
    items: Vec<TimelineItem>,
    max_items: usize,
}

impl TimelineBuffer {
    pub fn new(max_items: usize) -> Self {
        Self {
            items: Vec::new(),
            max_items: max_items.max(1),
        }
    }

    pub fn items(&self) -> &[TimelineItem] {
        &self.items
    }

    pub fn apply_ops(&mut self, ops: &[TimelineOp]) -> Result<(), TimelineMergeError> {
        for op in ops {
            match op {
                TimelineOp::Append(item) => self.items.push(item.clone()),
                TimelineOp::Prepend(item) => self.items.insert(0, item.clone()),
                TimelineOp::UpdateBody { event_id, new_body } => {
                    let item = self
                        .items
                        .iter_mut()
                        .find(|it| it.event_id.as_deref() == Some(event_id.as_str()))
                        .ok_or_else(|| TimelineMergeError::MissingEvent(event_id.clone()))?;
                    item.body = new_body.clone();
                }
                TimelineOp::Remove { event_id } => {
                    let idx = self
                        .items
                        .iter()
                        .position(|it| it.event_id.as_deref() == Some(event_id.as_str()))
                        .ok_or_else(|| TimelineMergeError::MissingEvent(event_id.clone()))?;
                    self.items.remove(idx);
                }
                TimelineOp::Clear => self.items.clear(),
            }
            self.trim_to_max();
        }

        Ok(())
    }

    pub fn bounded_paginate_limit(requested: u16, server_cap: u16) -> u16 {
        let safe_requested = requested.max(1);
        let safe_cap = server_cap.max(1);
        safe_requested.min(safe_cap).min(100)
    }

    fn trim_to_max(&mut self) {
        if self.items.len() <= self.max_items {
            return;
        }

        let excess = self.items.len() - self.max_items;
        self.items.drain(0..excess);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(event_id: &str, body: &str) -> TimelineItem {
        TimelineItem {
            event_id: Some(event_id.to_owned()),
            sender: "@alice:example.org".to_owned(),
            body: body.to_owned(),
            timestamp_ms: 1_731_000_000,
        }
    }

    #[test]
    fn applies_append_update_remove_sequence() {
        let mut timeline = TimelineBuffer::new(50);
        timeline
            .apply_ops(&[
                TimelineOp::Append(item("$1", "hello")),
                TimelineOp::Append(item("$2", "world")),
                TimelineOp::UpdateBody {
                    event_id: "$2".into(),
                    new_body: "world!".into(),
                },
                TimelineOp::Remove {
                    event_id: "$1".into(),
                },
            ])
            .expect("ops should be valid");

        assert_eq!(timeline.items().len(), 1);
        assert_eq!(timeline.items()[0].event_id.as_deref(), Some("$2"));
        assert_eq!(timeline.items()[0].body, "world!");
    }

    #[test]
    fn fails_when_event_for_update_is_missing() {
        let mut timeline = TimelineBuffer::new(10);
        let err = timeline
            .apply_ops(&[TimelineOp::UpdateBody {
                event_id: "$404".into(),
                new_body: "x".into(),
            }])
            .expect_err("should reject updates to unknown events");
        assert_eq!(err, TimelineMergeError::MissingEvent("$404".into()));
    }

    #[test]
    fn trims_oldest_when_over_max_items() {
        let mut timeline = TimelineBuffer::new(2);
        timeline
            .apply_ops(&[
                TimelineOp::Append(item("$1", "one")),
                TimelineOp::Append(item("$2", "two")),
                TimelineOp::Append(item("$3", "three")),
            ])
            .expect("append should work");

        assert_eq!(timeline.items().len(), 2);
        assert_eq!(timeline.items()[0].event_id.as_deref(), Some("$2"));
        assert_eq!(timeline.items()[1].event_id.as_deref(), Some("$3"));
    }

    #[test]
    fn bounds_paginate_limit_for_safety() {
        assert_eq!(TimelineBuffer::bounded_paginate_limit(0, 200), 1);
        assert_eq!(TimelineBuffer::bounded_paginate_limit(25, 10), 10);
        assert_eq!(TimelineBuffer::bounded_paginate_limit(150, 500), 100);
    }
}
