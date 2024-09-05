use std::collections::HashMap;
use std::ops::Bound;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use scru128::Scru128Id;

use serde::{Deserialize, Deserializer, Serialize};

use fjall::{Config, Keyspace, PartitionCreateOptions, PartitionHandle};

#[derive(PartialEq, Debug, Serialize, Deserialize, Clone, Default)]
pub struct Frame {
    pub id: Scru128Id,
    pub topic: String,
    pub hash: Option<ssri::Integrity>,
    pub meta: Option<serde_json::Value>,
}

#[derive(Clone)]
pub struct Store {
    pub path: PathBuf,
    // keep a reference to the keyspace, so we get a fsync when the store is dropped:
    // https://github.com/fjall-rs/fjall/discussions/44
    _keyspace: Keyspace,
    pub partition: PartitionHandle,
    commands_tx: tokio::sync::mpsc::Sender<Command>,
}

#[derive(Default, PartialEq, Clone, Debug)]
pub enum FollowOption {
    #[default]
    Off,
    On,
    WithHeartbeat(Duration),
}

impl<'de> Deserialize<'de> for FollowOption {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s: String = Deserialize::deserialize(deserializer)?;
        if s.is_empty() || s == "yes" {
            Ok(FollowOption::On)
        } else if let Ok(duration) = s.parse::<u64>() {
            Ok(FollowOption::WithHeartbeat(Duration::from_millis(duration)))
        } else {
            match s.as_str() {
                "true" => Ok(FollowOption::On),
                "false" | "no" => Ok(FollowOption::Off),
                _ => Err(serde::de::Error::custom("Invalid value for follow option")),
            }
        }
    }
}

fn deserialize_bool<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    let s: String = Deserialize::deserialize(deserializer)?;
    match s.as_str() {
        "false" | "no" | "0" => Ok(false),
        _ => Ok(true),
    }
}

#[derive(PartialEq, Deserialize, Clone, Debug, Default)]
#[bon::builder]
pub struct ReadOptions {
    #[serde(default)]
    #[builder(default)]
    pub follow: FollowOption,
    #[serde(default, deserialize_with = "deserialize_bool")]
    #[builder(default)]
    pub tail: bool,
    #[serde(rename = "last-id")]
    pub last_id: Option<Scru128Id>,
    #[serde(skip)]
    pub compaction_strategy: Option<fn(&Frame) -> Option<String>>,
}

impl ReadOptions {
    pub fn from_query(query: Option<&str>) -> Result<Self, serde_urlencoded::de::Error> {
        match query {
            Some(q) => serde_urlencoded::from_str(q),
            None => Ok(Self::default()),
        }
    }
}

#[derive(Debug)]
enum Command {
    Read(tokio::sync::mpsc::Sender<Frame>, ReadOptions),
    Append(Frame),
}

impl Store {
    pub fn spawn(path: PathBuf) -> Store {
        let config = Config::new(path.join("fjall"));
        let keyspace = config.open().unwrap();

        let partition = keyspace
            .open_partition("stream", PartitionCreateOptions::default())
            .unwrap();

        let (tx, rx) = tokio::sync::mpsc::channel::<Command>(32);

        let store = Store {
            path,
            _keyspace: keyspace,
            partition,
            commands_tx: tx,
        };

        let store_clone = store.clone();
        std::thread::spawn(move || {
            handle_commands(store_clone, rx);
        });

        store
    }

    pub async fn read(&self, options: ReadOptions) -> tokio::sync::mpsc::Receiver<Frame> {
        let (tx, rx) = tokio::sync::mpsc::channel::<Frame>(100);
        self.commands_tx
            .send(Command::Read(tx.clone(), options.clone()))
            .await
            .unwrap(); // our thread went away?

        if let FollowOption::WithHeartbeat(duration) = options.follow {
            tokio::task::spawn(async move {
                loop {
                    tokio::time::sleep(duration).await;
                    let frame = Frame {
                        id: scru128::new(),
                        topic: "xs.pulse".into(),
                        hash: None,
                        meta: None,
                    };
                    if tx.send(frame).await.is_err() {
                        break;
                    }
                }
            });
        }

        rx
    }

    pub fn get(&self, id: &Scru128Id) -> Option<Frame> {
        let res = self.partition.get(id.to_bytes()).unwrap();
        res.map(|value| serde_json::from_slice(&value).unwrap())
    }

    pub fn head(&self, topic: String) -> Option<Frame> {
        // Iterate over the partition in reverse order
        let range: (Bound<Vec<u8>>, Bound<Vec<u8>>) = (Bound::Unbounded, Bound::Unbounded);
        self.partition.range(range).rev().find_map(|record| {
            let (_, value) = record.ok()?;
            let frame: Frame = serde_json::from_slice(&value).ok()?;
            if frame.topic == topic {
                Some(frame)
            } else {
                None
            }
        })
    }

    pub async fn cas_reader(&self, hash: ssri::Integrity) -> cacache::Result<cacache::Reader> {
        cacache::Reader::open_hash(&self.path.join("cacache"), hash).await
    }

    pub async fn cas_writer(&self) -> cacache::Result<cacache::Writer> {
        cacache::WriteOpts::new()
            .open_hash(&self.path.join("cacache"))
            .await
    }

    pub async fn cas_insert(&self, content: &str) -> cacache::Result<ssri::Integrity> {
        cacache::write_hash(&self.path.join("cacache"), content).await
    }

    pub async fn cas_read(&self, hash: &ssri::Integrity) -> cacache::Result<Vec<u8>> {
        cacache::read_hash(&self.path.join("cacache"), hash).await
    }

    pub async fn append(
        &mut self,
        topic: &str,
        hash: Option<ssri::Integrity>,
        meta: Option<serde_json::Value>,
    ) -> Frame {
        // TODO: we shouldn't generate the id here, it should be generated by the command loop
        let frame = Frame {
            id: scru128::new(),
            topic: topic.to_string(),
            hash,
            meta: meta.clone(),
        };

        let encoded: Vec<u8> = serde_json::to_vec(&frame).unwrap();
        self.partition.insert(frame.id.to_bytes(), encoded).unwrap();

        self.commands_tx
            .send(Command::Append(frame.clone()))
            .await
            .unwrap(); // our thread went away?

        frame
    }

    pub async fn append_with_content(
        &mut self,
        topic: &str,
        content: &str,
        meta: Option<serde_json::Value>,
    ) -> Frame {
        self.append(topic, Some(self.cas_insert(content).await.unwrap()), meta)
            .await
    }
}

fn handle_commands(store: Store, mut rx: tokio::sync::mpsc::Receiver<Command>) {
    let mut subscribers: Vec<tokio::sync::mpsc::Sender<Frame>> = Vec::new();
    while let Some(command) = rx.blocking_recv() {
        match command {
            Command::Read(tx, options) => {
                let _ = handle_read_command(&store, &tx, &options, &mut subscribers);
            }
            Command::Append(frame) => {
                handle_append_command(&mut subscribers, frame);
            }
        }
    }
}

fn handle_read_command(
    store: &Store,
    tx: &tokio::sync::mpsc::Sender<Frame>,
    options: &ReadOptions,
    subscribers: &mut Vec<tokio::sync::mpsc::Sender<Frame>>,
) -> Result<(), tokio::sync::mpsc::error::SendError<Frame>> {
    if !options.tail {
        let range = get_range(options.last_id.as_ref());
        let mut compacted_frames = HashMap::new();

        for record in store.partition.range(range) {
            let frame = deserialize_frame(record.unwrap());

            if let Some(compaction_strategy) = &options.compaction_strategy {
                if let Some(key) = compaction_strategy(&frame) {
                    compacted_frames.insert(key, frame);
                }
            } else {
                send_frame(tx, frame)?;
            }
        }

        // Send compacted frames if a compaction strategy was used
        if !compacted_frames.is_empty() {
            for frame in compacted_frames.values() {
                send_frame(tx, frame.clone())?;
            }
        }
    }

    match options.follow {
        FollowOption::On | FollowOption::WithHeartbeat(_) => {
            if !options.tail && options.compaction_strategy.is_none() {
                send_frame(
                    tx,
                    Frame {
                        id: scru128::new(),
                        topic: "xs.threshold".into(),
                        hash: None,
                        meta: None,
                    },
                )?;
            }
            subscribers.push(tx.clone());
        }
        FollowOption::Off => {}
    };
    Ok(())
}

fn send_frame(
    tx: &tokio::sync::mpsc::Sender<Frame>,
    frame: Frame,
) -> Result<(), tokio::sync::mpsc::error::SendError<Frame>> {
    if let Err(e) = tx.blocking_send(frame) {
        tracing::error!("Failed to send frame: {:?}", e);
        return Err(e);
    }
    Ok(())
}

fn get_range(last_id: Option<&Scru128Id>) -> (Bound<Vec<u8>>, Bound<Vec<u8>>) {
    match last_id {
        Some(last_id) => (
            Bound::Excluded(last_id.as_bytes().to_vec()),
            Bound::Unbounded,
        ),
        None => (Bound::Unbounded, Bound::Unbounded),
    }
}

fn deserialize_frame(record: (Arc<[u8]>, Arc<[u8]>)) -> Frame {
    serde_json::from_slice(&record.1).unwrap_or_else(|e| {
        let key = std::str::from_utf8(&record.0).unwrap();
        let value = std::str::from_utf8(&record.1).unwrap();
        panic!("Failed to deserialize frame: {} {} {}", e, key, value)
    })
}

fn handle_append_command(subscribers: &mut Vec<tokio::sync::mpsc::Sender<Frame>>, frame: Frame) {
    subscribers.retain(|tx| {
        if tx.blocking_send(frame.clone()).is_ok() {
            true
        } else {
            tracing::error!("Subscriber not retained");
            false
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use static_assertions::assert_impl_all;

    #[test]
    fn test_store_is_send_sync() {
        assert_impl_all!(Store: Send, Sync);
    }
}

#[cfg(test)]
mod tests_read_options {
    use super::*;

    #[derive(Debug)]
    struct TestCase<'a> {
        input: Option<&'a str>,
        expected: ReadOptions,
    }

    #[test]
    fn test_from_query() {
        let test_cases = [
            TestCase {
                input: None,
                expected: ReadOptions::default(),
            },
            TestCase {
                input: Some("foo=bar"),
                expected: ReadOptions::default(),
            },
            TestCase {
                input: Some("follow"),
                expected: ReadOptions::builder().follow(FollowOption::On).build(),
            },
            TestCase {
                input: Some("follow=1"),
                expected: ReadOptions::builder()
                    .follow(FollowOption::WithHeartbeat(Duration::from_millis(1)))
                    .build(),
            },
            TestCase {
                input: Some("follow=yes"),
                expected: ReadOptions::builder().follow(FollowOption::On).build(),
            },
            TestCase {
                input: Some("follow=true"),
                expected: ReadOptions::builder().follow(FollowOption::On).build(),
            },
            TestCase {
                input: Some("last-id=03BIDZVKNOTGJPVUEW3K23G45"),
                expected: ReadOptions::builder()
                    .last_id("03BIDZVKNOTGJPVUEW3K23G45".parse().unwrap())
                    .build(),
            },
            TestCase {
                input: Some("follow&last-id=03BIDZVKNOTGJPVUEW3K23G45"),
                expected: ReadOptions::builder()
                    .follow(FollowOption::On)
                    .last_id("03BIDZVKNOTGJPVUEW3K23G45".parse().unwrap())
                    .build(),
            },
        ];

        for case in &test_cases {
            let options = ReadOptions::from_query(case.input);
            assert_eq!(options, Ok(case.expected.clone()), "case {:?}", case.input);
        }

        assert!(ReadOptions::from_query(Some("last-id=123")).is_err());
    }
}

#[cfg(test)]
mod tests_store {
    use super::*;
    use tempfile::TempDir;
    use tokio_stream::StreamExt;

    #[tokio::test]
    async fn test_get() {
        let temp_dir = TempDir::new().unwrap();
        let mut store = Store::spawn(temp_dir.into_path());
        let meta = serde_json::json!({"key": "value"});
        let frame = store.append("stream", None, Some(meta)).await;
        let got = store.get(&frame.id);
        assert_eq!(Some(frame.clone()), got);
    }

    #[tokio::test]
    async fn test_follow() {
        let temp_dir = TempDir::new().unwrap();
        let mut store = Store::spawn(temp_dir.into_path());

        // Append two initial clips
        let f1 = store.append("stream", None, None).await;
        let f2 = store.append("stream", None, None).await;

        // cat the full stream and follow new items with a heartbeat every 5ms
        let follow_options = ReadOptions::builder()
            .follow(FollowOption::WithHeartbeat(Duration::from_millis(5)))
            .build();
        let mut recver = store.read(follow_options).await;

        assert_eq!(f1, recver.recv().await.unwrap());
        assert_eq!(f2, recver.recv().await.unwrap());

        // crossing the threshold
        assert_eq!(
            "xs.threshold".to_string(),
            recver.recv().await.unwrap().topic
        );

        // Append two more clips
        let f3 = store.append("stream", None, None).await;
        let f4 = store.append("stream", None, None).await;
        assert_eq!(f3, recver.recv().await.unwrap());
        assert_eq!(f4, recver.recv().await.unwrap());
        let head = f4;

        // Assert we see some heartbeats
        assert_eq!("xs.pulse".to_string(), recver.recv().await.unwrap().topic);
        assert_eq!("xs.pulse".to_string(), recver.recv().await.unwrap().topic);

        // start a new subscriber to exercise compaction_strategy
        let follow_options = ReadOptions::builder()
            .follow(FollowOption::WithHeartbeat(Duration::from_millis(5)))
            .compaction_strategy(|frame| Some(frame.topic.clone()))
            .build();
        let mut recver = store.read(follow_options).await;

        assert_eq!(head, recver.recv().await.unwrap());

        // Assert we see some heartbeats - note we don't see xs.threshold
        assert_eq!("xs.pulse".to_string(), recver.recv().await.unwrap().topic);
        assert_eq!("xs.pulse".to_string(), recver.recv().await.unwrap().topic);
    }

    #[tokio::test]
    async fn test_stream_basics() {
        let temp_dir = TempDir::new().unwrap();
        let mut store = Store::spawn(temp_dir.into_path());

        let f1 = store.append("/stream", None, None).await;
        let f2 = store.append("/stream", None, None).await;

        assert_eq!(store.head("/stream".to_string()), Some(f2.clone()));

        let recver = store.read(ReadOptions::default()).await;
        assert_eq!(
            tokio_stream::wrappers::ReceiverStream::new(recver)
                .collect::<Vec<Frame>>()
                .await,
            vec![f1.clone(), f2.clone()]
        );

        let recver = store
            .read(ReadOptions::builder().last_id(f1.id).build())
            .await;
        assert_eq!(
            tokio_stream::wrappers::ReceiverStream::new(recver)
                .collect::<Vec<Frame>>()
                .await,
            vec![f2]
        );
    }
}
