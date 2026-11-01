//! What hypertop knows: devices, as described by whichever source reported them.
//! Nothing here knows what any kind of device is. A device has a name, a state,
//! numbers, things to peek at and things to do, all of them given by its source.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::time::Instant;

use serde_json::Value;

const HISTORY: usize = 60;
/// Numbers worth interrupting for, whatever the device.
const LIMITS: [(&str, f64, &str); 4] =
    [("disk", 90.0, "disk"), ("mem", 95.0, "memory"), ("temp", 85.0, "temperature"), ("gputemp", 85.0, "GPU temperature")];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Up,
    Idle,
    Down,
    Unknown,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Action {
    pub id: String,
    pub label: String,
    pub run: Vec<String>,
    /// Takes over the terminal (a shell), so it runs on one device at a time.
    pub tty: bool,
    pub confirm: bool,
    /// Ask for a line of text first, and put it where the command says {input}.
    pub input: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Peek {
    pub label: String,
    pub run: Vec<String>,
    pub every: u64,
}

pub struct Device {
    pub key: String,
    pub source: String,
    pub id: String,
    pub name: String,
    pub group: String,
    pub kind: String,
    pub state: State,
    pub note: String,
    pub alert: Option<String>,
    pub metrics: BTreeMap<String, f64>,
    pub info: BTreeMap<String, String>,
    pub history: BTreeMap<String, VecDeque<f32>>,
    pub peeks: Vec<Peek>,
    pub actions: Vec<Action>,
    pub tagged: bool,
    pub since: Instant,
}

fn strings(value: &Value) -> Vec<String> {
    value.as_array().map_or(Vec::new(), |items| items.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
}

fn parse_state(text: &str) -> State {
    match text {
        "up" => State::Up,
        "idle" => State::Idle,
        "down" => State::Down,
        _ => State::Unknown,
    }
}

impl Device {
    fn new(source: &str, id: &str) -> Device {
        Device {
            key: format!("{source}/{id}"),
            source: source.to_string(),
            id: id.to_string(),
            name: id.to_string(),
            group: source.to_string(),
            kind: String::new(),
            state: State::Unknown,
            note: String::new(),
            alert: None,
            metrics: BTreeMap::new(),
            info: BTreeMap::new(),
            history: BTreeMap::new(),
            peeks: Vec::new(),
            actions: Vec::new(),
            tagged: false,
            since: Instant::now(),
        }
    }

    /// Fields present in a line replace what was known; the rest stays, so a
    /// source can describe a device once and then send only its numbers.
    fn merge(&mut self, line: &Value) {
        if let Some(text) = line["name"].as_str() {
            self.name = text.to_string();
        }
        if let Some(text) = line["group"].as_str() {
            self.group = text.to_string();
        }
        if let Some(text) = line["kind"].as_str() {
            self.kind = text.to_string();
        }
        if let Some(text) = line["state"].as_str() {
            let state = parse_state(text);
            if state != self.state {
                self.state = state;
                self.since = Instant::now();
            }
        }
        if !line["note"].is_null() {
            self.note = line["note"].as_str().unwrap_or("").to_string();
        }
        if let Some(present) = line.as_object().and_then(|o| o.get("alert")) {
            self.alert = present.as_str().filter(|s| !s.is_empty()).map(str::to_string);
        }
        if let Some(metrics) = line["metrics"].as_object() {
            self.metrics.clear();
            for (name, value) in metrics {
                if let Some(number) = value.as_f64() {
                    self.metrics.insert(name.clone(), number);
                    let history = self.history.entry(name.clone()).or_default();
                    history.push_back(number as f32);
                    if history.len() > HISTORY {
                        history.pop_front();
                    }
                }
            }
        }
        if let Some(info) = line["info"].as_object() {
            self.info = info
                .iter()
                .map(|(k, v)| (k.clone(), v.as_str().map_or_else(|| v.to_string(), str::to_string)))
                .collect();
        }
        if let Some(items) = line["peeks"].as_array() {
            self.peeks = items
                .iter()
                .filter_map(|p| {
                    let run = strings(&p["run"]);
                    (!run.is_empty()).then(|| Peek {
                        label: p["label"].as_str().unwrap_or("peek").to_string(),
                        run,
                        every: p["every"].as_u64().unwrap_or(3).max(1),
                    })
                })
                .collect();
        }
        if let Some(items) = line["actions"].as_array() {
            self.actions = items
                .iter()
                .filter_map(|a| {
                    let run = strings(&a["run"]);
                    let id = a["id"].as_str()?.to_string();
                    (!run.is_empty()).then(|| Action {
                        label: a["label"].as_str().unwrap_or(&id).to_string(),
                        id,
                        run,
                        tty: a["tty"].as_bool().unwrap_or(false),
                        confirm: a["confirm"].as_bool().unwrap_or(false),
                        input: a["input"].as_str().map(str::to_string),
                    })
                })
                .collect();
        }
    }

    /// Why this device wants looking at, if it does.
    pub fn attention(&self) -> Option<String> {
        if self.alert.is_some() {
            return self.alert.clone();
        }
        if self.state != State::Up {
            return None;
        }
        LIMITS.iter().find_map(|(metric, limit, label)| {
            let value = *self.metrics.get(*metric)?;
            (value >= *limit).then(|| format!("{label} {value:.0}"))
        })
    }

    fn matches(&self, filter: &str) -> bool {
        if filter.is_empty() {
            return true;
        }
        let filter = filter.to_lowercase();
        [&self.name, &self.kind, &self.group, &self.note]
            .into_iter()
            .chain(self.info.values())
            .any(|text| text.to_lowercase().contains(&filter))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GroupBy {
    Group,
    Kind,
    Nothing,
}

#[derive(Clone, PartialEq, Debug)]
pub enum Row {
    Header { title: String, up: usize, total: usize },
    Device(String),
}

/// The name a failed source goes by in the list.
const SOURCE_FAILED: &str = "!";

#[derive(Default)]
pub struct Model {
    pub devices: Vec<Device>,
    seen_this_run: HashMap<String, HashSet<String>>,
}

impl Model {
    pub fn get(&self, key: &str) -> Option<&Device> {
        self.devices.iter().find(|d| d.key == key)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Device> {
        self.devices.iter_mut().find(|d| d.key == key)
    }

    /// One line from a source. A line with no id says nothing about any device.
    pub fn line(&mut self, source: &str, line: &Value) {
        let id = match &line["id"] {
            Value::String(id) => id.clone(),
            Value::Number(id) => id.to_string(),
            _ => return,
        };
        self.seen_this_run.entry(source.to_string()).or_default().insert(id.clone());
        let key = format!("{source}/{id}");
        let at = match self.devices.iter().position(|d| d.key == key) {
            Some(at) => at,
            None => {
                self.devices.push(Device::new(source, &id));
                self.devices.len() - 1
            }
        };
        self.devices[at].merge(line);
    }

    /// A source's run ended. If it succeeded, devices it no longer mentioned are
    /// gone. If it failed, its devices stay as they were, and a row says so: a
    /// source failing must never look like its devices being fine.
    pub fn source_exited(&mut self, source: &str, success: bool, why: &str) {
        let seen = self.seen_this_run.remove(source).unwrap_or_default();
        let failed_key = format!("{source}/{SOURCE_FAILED}");
        if success {
            self.devices.retain(|d| d.source != source || seen.contains(&d.id));
        } else {
            self.devices.retain(|d| d.key != failed_key);
            let mut device = Device::new(source, SOURCE_FAILED);
            device.name = source.to_string();
            device.group = "sources".to_string();
            device.kind = "source".to_string();
            device.alert = Some(if why.is_empty() { "failed".to_string() } else { why.to_string() });
            self.devices.push(device);
        }
    }

    pub fn counts(&self) -> (usize, usize, usize, usize) {
        let up = self.devices.iter().filter(|d| d.state == State::Up).count();
        let down = self.devices.iter().filter(|d| d.state == State::Down).count();
        let attention = self.devices.iter().filter(|d| d.attention().is_some()).count();
        (self.devices.len(), up, down, attention)
    }

    /// What's on screen, in order. Sorted by name only, so a row stays where you
    /// last saw it when its numbers change.
    pub fn rows(&self, filter: &str, hide_down: bool, group_by: GroupBy) -> Vec<Row> {
        let mut shown: Vec<&Device> = self
            .devices
            .iter()
            .filter(|d| d.matches(filter) && (!hide_down || d.state != State::Down))
            .collect();
        let section = |d: &Device| match group_by {
            GroupBy::Group => d.group.clone(),
            GroupBy::Kind => d.kind.clone(),
            GroupBy::Nothing => String::new(),
        };
        shown.sort_by_key(|d| (section(d).to_lowercase(), d.name.to_lowercase(), d.key.clone()));
        let mut rows = Vec::with_capacity(shown.len() + 8);
        let mut current = None;
        for device in &shown {
            let title = section(device);
            if group_by != GroupBy::Nothing && current.as_ref() != Some(&title) {
                let members = shown.iter().filter(|d| section(d) == title);
                let total = members.clone().count();
                let up = members.filter(|d| d.state == State::Up).count();
                rows.push(Row::Header { title: title.clone(), up, total });
                current = Some(title);
            }
            rows.push(Row::Device(device.key.clone()));
        }
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn model() -> Model {
        let mut m = Model::default();
        m.line("s", &json!({"id": "b", "group": "g1", "kind": "k", "state": "up", "metrics": {"cpu": 10.0}}));
        m.line("s", &json!({"id": "a", "group": "g1", "kind": "k", "state": "down"}));
        m.line("t", &json!({"id": "c", "group": "g2", "state": "idle", "note": "hello"}));
        m
    }

    #[test]
    fn a_line_only_changes_what_it_mentions() {
        let mut m = model();
        m.line("s", &json!({"id": "b", "metrics": {"cpu": 20.0}}));
        let d = m.get("s/b").unwrap();
        assert_eq!(d.state, State::Up);
        assert_eq!(d.group, "g1");
        assert_eq!(d.metrics["cpu"], 20.0);
        assert_eq!(d.history["cpu"], VecDeque::from([10.0, 20.0]));
    }

    #[test]
    fn the_same_id_from_two_sources_is_two_devices() {
        let mut m = Model::default();
        m.line("s", &json!({"id": "x"}));
        m.line("t", &json!({"id": "x"}));
        assert_eq!(m.devices.len(), 2);
    }

    #[test]
    fn a_line_without_an_id_is_ignored() {
        let mut m = Model::default();
        m.line("s", &json!({"name": "nobody"}));
        assert!(m.devices.is_empty());
    }

    #[test]
    fn rows_group_and_sort_by_name() {
        let rows = model().rows("", false, GroupBy::Group);
        let names: Vec<String> = rows
            .iter()
            .map(|r| match r {
                Row::Header { title, up, total } => format!("#{title} {up}/{total}"),
                Row::Device(key) => key.clone(),
            })
            .collect();
        assert_eq!(names, ["#g1 1/2", "s/a", "s/b", "#g2 0/1", "t/c"]);
    }

    #[test]
    fn filter_and_hide_down() {
        let m = model();
        assert_eq!(m.rows("HELLO", false, GroupBy::Nothing), vec![Row::Device("t/c".into())]);
        assert!(!m.rows("", true, GroupBy::Nothing).contains(&Row::Device("s/a".into())));
    }

    #[test]
    fn numbers_past_a_limit_want_attention_only_while_up() {
        let mut m = Model::default();
        m.line("s", &json!({"id": "x", "state": "up", "metrics": {"disk": 97.0}}));
        assert_eq!(m.get("s/x").unwrap().attention().as_deref(), Some("disk 97"));
        m.line("s", &json!({"id": "x", "state": "down"}));
        assert_eq!(m.get("s/x").unwrap().attention(), None);
    }

    #[test]
    fn alert_can_be_set_and_cleared() {
        let mut m = Model::default();
        m.line("s", &json!({"id": "x", "alert": "on fire"}));
        assert_eq!(m.get("s/x").unwrap().attention().as_deref(), Some("on fire"));
        m.line("s", &json!({"id": "x", "alert": null}));
        assert_eq!(m.get("s/x").unwrap().attention(), None);
    }

    #[test]
    fn a_successful_run_forgets_what_it_no_longer_mentions() {
        let mut m = model();
        m.source_exited("s", true, ""); // saw both
        assert_eq!(m.devices.len(), 3);
        m.line("s", &json!({"id": "b"}));
        m.source_exited("s", true, "");
        assert!(m.get("s/a").is_none());
        assert!(m.get("s/b").is_some());
    }

    #[test]
    fn a_failed_run_keeps_devices_and_says_so() {
        let mut m = model();
        m.source_exited("s", false, "no such tool");
        assert!(m.get("s/a").is_some());
        assert_eq!(m.get("s/!").unwrap().attention().as_deref(), Some("no such tool"));
        m.line("s", &json!({"id": "a"}));
        m.line("s", &json!({"id": "b"}));
        m.source_exited("s", true, "");
        assert!(m.get("s/!").is_none(), "recovered");
    }

    #[test]
    fn actions_and_peeks_need_a_command() {
        let mut m = Model::default();
        m.line("s", &json!({"id": "x",
            "actions": [{"id": "ok", "run": ["true"], "confirm": true}, {"id": "empty", "run": []}, {"run": ["no-id"]}],
            "peeks": [{"label": "p", "run": ["true"]}, {"label": "empty", "run": []}]}));
        let d = m.get("s/x").unwrap();
        assert_eq!(d.actions.len(), 1);
        assert!(d.actions[0].confirm);
        assert_eq!(d.actions[0].label, "ok");
        assert_eq!(d.peeks.len(), 1);
        assert_eq!(d.peeks[0].every, 3);
    }
}
