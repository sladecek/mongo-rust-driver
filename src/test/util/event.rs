use std::{
    collections::VecDeque,
    sync::{Arc, RwLock},
    time::Duration,
};

use tokio::sync::RwLockReadGuard;

use super::TestClient;
use crate::{
    bson::doc,
    event::{
        cmap::{CmapEventHandler, PoolClearedEvent},
        command::{
            CommandEventHandler,
            CommandFailedEvent,
            CommandStartedEvent,
            CommandSucceededEvent,
        },
    },
    options::ClientOptions,
    test::{CLIENT_OPTIONS, LOCK},
};

pub type EventQueue<T> = Arc<RwLock<VecDeque<T>>>;

#[derive(Clone, Debug)]
pub enum CommandEvent {
    CommandStartedEvent(CommandStartedEvent),
    CommandSucceededEvent(CommandSucceededEvent),
    CommandFailedEvent(CommandFailedEvent),
}

impl CommandEvent {
    pub fn command_name(&self) -> &str {
        match self {
            CommandEvent::CommandStartedEvent(event) => event.command_name.as_str(),
            CommandEvent::CommandFailedEvent(event) => event.command_name.as_str(),
            CommandEvent::CommandSucceededEvent(event) => event.command_name.as_str(),
        }
    }

    pub fn is_command_started(&self) -> bool {
        matches!(self, CommandEvent::CommandStartedEvent(_))
    }

    fn request_id(&self) -> i32 {
        match self {
            CommandEvent::CommandStartedEvent(event) => event.request_id,
            CommandEvent::CommandFailedEvent(event) => event.request_id,
            CommandEvent::CommandSucceededEvent(event) => event.request_id,
        }
    }

    fn as_command_started(&self) -> Option<&CommandStartedEvent> {
        match self {
            CommandEvent::CommandStartedEvent(e) => Some(e),
            _ => None,
        }
    }

    fn as_command_succeeded(&self) -> Option<&CommandSucceededEvent> {
        match self {
            CommandEvent::CommandSucceededEvent(e) => Some(e),
            _ => None,
        }
    }
}

#[derive(Default)]
pub struct EventHandler {
    pub command_events: EventQueue<CommandEvent>,
    pub pool_cleared_events: EventQueue<PoolClearedEvent>,
}

impl CmapEventHandler for EventHandler {
    fn handle_pool_cleared_event(&self, event: PoolClearedEvent) {
        self.pool_cleared_events.write().unwrap().push_back(event)
    }
}

impl CommandEventHandler for EventHandler {
    fn handle_command_started_event(&self, event: CommandStartedEvent) {
        self.command_events
            .write()
            .unwrap()
            .push_back(CommandEvent::CommandStartedEvent(event))
    }

    fn handle_command_failed_event(&self, event: CommandFailedEvent) {
        self.command_events
            .write()
            .unwrap()
            .push_back(CommandEvent::CommandFailedEvent(event))
    }

    fn handle_command_succeeded_event(&self, event: CommandSucceededEvent) {
        self.command_events
            .write()
            .unwrap()
            .push_back(CommandEvent::CommandSucceededEvent(event))
    }
}

#[derive(Clone, Debug)]
pub struct EventClient {
    client: TestClient,
    pub command_events: EventQueue<CommandEvent>,
    pub pool_cleared_events: EventQueue<PoolClearedEvent>,
}

impl std::ops::Deref for EventClient {
    type Target = TestClient;

    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

impl std::ops::DerefMut for EventClient {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.client
    }
}

impl EventClient {
    pub async fn new() -> Self {
        EventClient::with_options(None).await
    }

    pub async fn with_options(options: impl Into<Option<ClientOptions>>) -> Self {
        let handler = EventHandler::default();
        let command_events = handler.command_events.clone();
        let pool_cleared_events = handler.pool_cleared_events.clone();
        let client = TestClient::with_handler(Some(handler), options).await;

        // clear events from commands used to set up client.
        command_events.write().unwrap().clear();

        Self {
            client,
            command_events,
            pool_cleared_events,
        }
    }

    pub async fn with_additional_options(
        options: Option<ClientOptions>,
        heartbeat_freq: Option<Duration>,
        use_multiple_mongoses: Option<bool>,
    ) -> Self {
        let mut options = match options {
            Some(mut options) => {
                options.hosts = CLIENT_OPTIONS.hosts.clone();
                options.merge(CLIENT_OPTIONS.clone());
                options
            }
            None => CLIENT_OPTIONS.clone(),
        };
        options.heartbeat_freq_test = heartbeat_freq;
        if TestClient::new().await.is_sharded() && use_multiple_mongoses != Some(true) {
            options.hosts = options.hosts.iter().cloned().take(1).collect();
        }
        EventClient::with_options(options).await
    }

    pub async fn with_uri_and_mongos_options(
        uri: &str,
        use_multiple_mongoses: Option<bool>,
    ) -> Self {
        let mut options = ClientOptions::parse_uri(uri, None).await.unwrap();
        match use_multiple_mongoses {
            Some(true) => {
                if options.hosts.len() <= 1 {
                    panic!("Test requires multiple mongos hosts");
                }
            }
            Some(false) => {
                options.hosts = options.hosts.iter().cloned().take(1).collect();
            }
            None => {}
        }
        EventClient::with_options(options).await
    }

    /// Gets the first started/succeeded pair of events for the given command name, popping off all
    /// events before and between them.
    ///
    /// Panics if the command failed or could not be found in the events.
    pub fn get_successful_command_execution(
        &self,
        command_name: &str,
    ) -> (CommandStartedEvent, CommandSucceededEvent) {
        let mut command_events = self.command_events.write().unwrap();

        let mut started: Option<CommandStartedEvent> = None;

        while let Some(event) = command_events.pop_front() {
            if event.command_name() == command_name {
                match started {
                    None => {
                        let event = event
                            .as_command_started()
                            .unwrap_or_else(|| {
                                panic!("first event not a command started event {:?}", event)
                            })
                            .clone();
                        started = Some(event);
                        continue;
                    }
                    Some(started) if event.request_id() == started.request_id => {
                        let succeeded = event
                            .as_command_succeeded()
                            .expect("second event not a command succeeded event")
                            .clone();

                        return (started, succeeded);
                    }
                    _ => continue,
                }
            }
        }
        panic!("could not find event for {} command", command_name);
    }

    pub fn topology(&self) -> String {
        if self.client.is_sharded() {
            String::from("sharded")
        } else if self.client.is_replica_set() {
            String::from("replicaset")
        } else {
            String::from("single")
        }
    }

    /// Gets all of the command started events for a specified command name.
    pub fn get_command_started_events(&self, command_name: &str) -> Vec<CommandStartedEvent> {
        let events = self.command_events.read().unwrap();
        events
            .iter()
            .filter_map(|event| match event {
                CommandEvent::CommandStartedEvent(event) => {
                    if event.command_name == command_name {
                        Some(event.clone())
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .collect()
    }

    /// Gets a list of all of the events of the requested event types that occurred on this client.
    /// Ignores any event with a name in the ignore list. Also ignores all configureFailPoint
    /// events.
    pub fn get_filtered_events(
        &self,
        observe_events: &Option<Vec<String>>,
        ignore_command_names: &Option<Vec<String>>,
    ) -> Vec<CommandEvent> {
        let events = self.command_events.read().unwrap();
        events
            .iter()
            .cloned()
            .filter(|event| {
                if event.command_name() == "configureFailPoint" {
                    return false;
                }
                if let Some(observe_events) = observe_events {
                    if !observe_events.iter().any(|name| match event {
                        CommandEvent::CommandStartedEvent(_) => {
                            name.as_str() == "commandStartedEvent"
                        }
                        CommandEvent::CommandSucceededEvent(_) => {
                            name.as_str() == "commandSucceededEvent"
                        }
                        CommandEvent::CommandFailedEvent(_) => {
                            name.as_str() == "commandFailedEvent"
                        }
                    }) {
                        return false;
                    }
                }
                if let Some(ignore_command_names) = ignore_command_names {
                    if ignore_command_names
                        .iter()
                        .any(|name| event.command_name() == name)
                    {
                        return false;
                    }
                }
                true
            })
            .collect()
    }
}

#[cfg_attr(feature = "tokio-runtime", tokio::test)]
#[cfg_attr(feature = "async-std-runtime", async_std::test)]
async fn command_started_event_count() {
    let _guard: RwLockReadGuard<()> = LOCK.run_concurrently().await;

    let client = EventClient::new().await;
    let coll = client.database("foo").collection("bar");

    for i in 0..10 {
        coll.insert_one(doc! { "x": i }, None).await.unwrap();
    }

    assert_eq!(
        client
            .command_events
            .read()
            .unwrap()
            .iter()
            .filter(|event| event.is_command_started() && event.command_name() == "insert")
            .count(),
        10
    );
}
