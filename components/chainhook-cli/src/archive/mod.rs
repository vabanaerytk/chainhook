use crate::config::Config;
use chainhook_types::StacksNetwork;
use flate2::read::GzDecoder;
use futures_util::StreamExt;
use std::fs;
use std::io::Read;
use std::io::{self, Cursor};
use tar::Archive;

pub fn default_tsv_file_path(network: &StacksNetwork) -> String {
    format!("stacks-node-events-{:?}.tsv", network).to_lowercase()    
}

pub async fn download_tsv_file(config: &Config) -> Result<(), String> {
    let destination_path = config.expected_cache_path();
    let url = config.expected_remote_tsv_url();
    let res = reqwest::get(url)
        .await
        .or(Err(format!("Failed to GET from '{}'", &url)))?;

    // Download chunks
    let (tx, rx) = flume::bounded(0);

    let mut from = destination_path.clone();
    let decoder_thread = std::thread::spawn(move || {
        let input = ChannelRead::new(rx);
        let gz = GzDecoder::new(input);
        let mut archive = Archive::new(gz);
        archive.unpack(&destination_path).unwrap();
    });

    if res.status() == reqwest::StatusCode::OK {
        let mut stream = res.bytes_stream();
        while let Some(item) = stream.next().await {
            let chunk = item
                .or(Err(format!("Error while downloading file")))
                .unwrap();
            tx.send_async(chunk.to_vec()).await.unwrap();
        }
        drop(tx);
    }

    tokio::task::spawn_blocking(|| decoder_thread.join())
        .await
        .unwrap()
        .unwrap();

    from.push("stacks-node-events.tsv");
    let mut to = from.clone();
    to.pop();
    to.push(default_tsv_file_path(&config.network.stacks_network));
    let _ = fs::rename(from, to);

    Ok(())
}

// Wrap a channel into something that impls `io::Read`
struct ChannelRead {
    rx: flume::Receiver<Vec<u8>>,
    current: Cursor<Vec<u8>>,
}

impl ChannelRead {
    fn new(rx: flume::Receiver<Vec<u8>>) -> ChannelRead {
        ChannelRead {
            rx,
            current: Cursor::new(vec![]),
        }
    }
}

impl Read for ChannelRead {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.current.position() == self.current.get_ref().len() as u64 {
            // We've exhausted the previous chunk, get a new one.
            if let Ok(vec) = self.rx.recv() {
                self.current = io::Cursor::new(vec);
            }
            // If recv() "fails", it means the sender closed its part of
            // the channel, which means EOF. Propagate EOF by allowing
            // a read from the exhausted cursor.
        }
        self.current.read(buf)
    }
}
