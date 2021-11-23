use std::convert::TryFrom;
use std::option::Option;

use crate::bindings::Bindings;
use crate::sources::Source;
use crate::terms::{Term, ToPolarString};
use crate::vm;

use serde;
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub struct Event {
    timestamp_ms: u128,
    id: u64,
    parent_id: u64,
    #[serde(flatten)]
    event_type: EventDetail,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "goal_type")]
pub enum Goal {
    Query { term: Term, polar: String },
}

impl TryFrom<vm::Goal> for Goal {
    type Error = ();

    fn try_from(other: vm::Goal) -> Result<Self, ()> {
        match other {
            vm::Goal::Query { term } => {
                let polar = term.to_polar();
                Ok(Goal::Query { term, polar })
            }
            _ => Err(()),
        }
    }
}

impl Event {
    pub fn execute_goal(goal: vm::Goal, source: Option<Source>) -> Self {
        let goal = Goal::try_from(goal).unwrap();

        Event {
            timestamp_ms: _timestamp_ms(),
            id: 0,
            parent_id: 0,
            event_type: EventDetail::ExecuteGoal { goal, source },
        }
    }

    pub fn evaluate_rule(rule: String, source: Option<Source>) -> Self {
        Event {
            timestamp_ms: _timestamp_ms(),
            id: 0,
            parent_id: 0,
            event_type: EventDetail::EvaluateRule { rule, source },
        }
    }

    pub fn backtrack(reason: String) -> Self {
        Event {
            timestamp_ms: _timestamp_ms(),
            id: 0,
            parent_id: 0,
            event_type: EventDetail::Backtrack { reason },
        }
    }

    pub fn result(bindings: Bindings) -> Self {
        Event {
            timestamp_ms: _timestamp_ms(),
            id: 0,
            parent_id: 0,
            event_type: EventDetail::Result { bindings },
        }
    }

    pub fn done() -> Self {
        Event {
            timestamp_ms: _timestamp_ms(),
            id: 0,
            parent_id: 0,
            event_type: EventDetail::Done {},
        }
    }

    pub fn choice_push() -> Self {
        Event {
            timestamp_ms: _timestamp_ms(),
            id: 0,
            parent_id: 0,
            event_type: EventDetail::ChoicePush {},
        }
    }

    pub fn execute_choice() -> Self {
        Event {
            timestamp_ms: _timestamp_ms(),
            id: 0,
            parent_id: 0,
            event_type: EventDetail::ExecuteChoice {},
        }
    }

    pub fn bindings(bindings: Bindings) -> Event {
        Event {
            timestamp_ms: _timestamp_ms(),
            id: 0,
            parent_id: 0,
            event_type: EventDetail::Bindings { bindings },
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "event_type")]
pub enum EventDetail {
    EvaluateRule {
        rule: String,
        source: Option<Source>,
    },
    Backtrack {
        reason: String,
    },
    Result {
        bindings: Bindings,
    },
    Done {},
    ExecuteGoal {
        goal: Goal,
        source: Option<Source>,
    },
    ChoicePush {},
    ExecuteChoice {},
    Bindings {
        bindings: Bindings,
    },
}

/// Recorder to gather Trace events
#[derive(Clone, Default)]
pub struct Recorder {
    events: Vec<Event>,
    next_id: u64,
}

impl Recorder {
    fn push(&mut self, mut event: Event) -> u64 {
        let id = self.next_id;
        self.next_id += 1;

        event.id = id;
        self.events.push(event);

        id
    }

    fn events(&self) -> &Vec<Event> {
        &self.events
    }
}

#[derive(Clone, Default)]
pub struct ScopedRecorder {
    recorder: Recorder,
    parent_id: Vec<u64>,
}

impl ScopedRecorder {
    fn parent_id(&self) -> u64 {
        self.parent_id.last().cloned().unwrap_or_default()
    }

    pub fn push_parent(&mut self, event: Event) -> u64 {
        let id = self.push(event);
        self.parent_id.push(id);
        id
    }

    pub fn push(&mut self, mut event: Event) -> u64 {
        event.parent_id = self.parent_id();
        self.recorder.push(event)
    }

    pub fn pop_up_to(&mut self, target: u64) {
        loop {
            let id = self.parent_id.last().unwrap();
            if id == &target {
                return;
            }

            self.parent_id.pop();
        }
    }

    pub fn pop_to(&mut self, target: u64) {
        loop {
            let id = self.parent_id.pop().unwrap();
            if id == target {
                return;
            }
        }
    }

    pub fn events(&self) -> &Vec<Event> {
        self.recorder.events()
    }
}

fn _timestamp_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
}
