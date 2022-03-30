extern crate core;

use std::collections::BTreeMap;
use std::io::{stdout, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::ops::Bound::Included;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use sha2::digest::Digest;
use sha2::Sha512;

use syncwhole::datastore::{DataStore, LoadResult, StoreResult};
use syncwhole::host::Host;
use syncwhole::node::{Node, RemoteNodeInfo};
use syncwhole::utils::*;

const TEST_NODE_COUNT: usize = 8;
const TEST_PORT_RANGE_START: u16 = 21384;
const TEST_STARTING_RECORDS_PER_NODE: usize = 16;

static mut RANDOM_CTR: u128 = 0;

fn get_random_bytes(mut buf: &mut [u8]) {
    // This is only for testing and is not really secure.
    let mut ctr = unsafe { RANDOM_CTR };
    if ctr == 0 {
        ctr = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos() * (1 + Instant::now().elapsed().as_nanos());
    }
    while !buf.is_empty() {
        let l = buf.len().min(64);
        ctr = ctr.wrapping_add(1);
        buf[..l].copy_from_slice(&Sha512::digest(&ctr.to_ne_bytes()).as_slice()[..l]);
        buf = &mut buf[l..];
    }
    unsafe { RANDOM_CTR = ctr };
}

struct TestNodeHost {
    name: String,
    peers: Vec<SocketAddr>,
    db: Mutex<BTreeMap<[u8; 64], Arc<[u8]>>>,
}

impl Host for TestNodeHost {
    fn fixed_peers(&self) -> &[SocketAddr] { self.peers.as_slice() }

    fn name(&self) -> Option<&str> { Some(self.name.as_str()) }

    fn on_connect_attempt(&self, _address: &SocketAddr) {
        //println!("{:5}: connecting to {}", self.name, _address.to_string());
    }

    fn on_connect(&self, info: &RemoteNodeInfo) {
        //println!("{:5}: connected to {} ({}, {})", self.name, info.remote_address.to_string(), info.node_name.as_ref().map_or("null", |s| s.as_str()), if info.inbound { "inbound" } else { "outbound" });
    }

    fn on_connection_closed(&self, info: &RemoteNodeInfo, reason: String) {
        println!("{:5}: closed connection to {}: {} ({}, {})", self.name, info.remote_address.to_string(), reason, if info.inbound { "inbound" } else { "outbound" }, if info.initialized { "initialized" } else { "not initialized" });
    }

    fn get_secure_random(&self, buf: &mut [u8]) {
        // This is only for testing and is not really secure.
        get_random_bytes(buf);
    }
}

impl DataStore for TestNodeHost {
    type LoadResultValueType = Arc<[u8]>;

    const MAX_VALUE_SIZE: usize = 1024;

    fn clock(&self) -> i64 { ms_since_epoch() }

    fn domain(&self) -> &str { "test" }

    fn load(&self, _: i64, key: &[u8]) -> LoadResult<Self::LoadResultValueType> {
        self.db.lock().unwrap().get(key).map_or(LoadResult::NotFound, |r| LoadResult::Ok(r.clone()))
    }

    fn store(&self, key: &[u8], value: &[u8]) -> StoreResult {
        assert_eq!(key.len(), 64);
        let mut res = StoreResult::Ok(0);
        self.db.lock().unwrap().entry(key.try_into().unwrap()).and_modify(|e| {
            if e.as_ref().eq(value) {
                res = StoreResult::Duplicate;
            } else {
                *e = Arc::from(value)
            }
        }).or_insert_with(|| {
            Arc::from(value)
        });
        res
    }

    fn count(&self, _: i64, key_range_start: &[u8], key_range_end: &[u8]) -> u64 {
        let s: [u8; 64] = key_range_start.try_into().unwrap();
        let e: [u8; 64] = key_range_end.try_into().unwrap();
        self.db.lock().unwrap().range((Included(s), Included(e))).count() as u64
    }

    fn total_count(&self) -> u64 { self.db.lock().unwrap().len() as u64 }

    fn for_each<F: FnMut(&[u8], &[u8]) -> bool>(&self, _: i64, key_range_start: &[u8], key_range_end: &[u8], mut f: F) {
        let s: [u8; 64] = key_range_start.try_into().unwrap();
        let e: [u8; 64] = key_range_end.try_into().unwrap();
        for (k, v) in self.db.lock().unwrap().range((Included(s), Included(e))) {
            if !f(k, v.as_ref()) {
                break;
            }
        }
    }
}

fn main() {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        println!("Running syncwhole local self-test network with {} nodes starting at 127.0.0.1:{}", TEST_NODE_COUNT, TEST_PORT_RANGE_START);
        println!();

        println!("Starting nodes on 127.0.0.1...");
        let mut nodes: Vec<Node<TestNodeHost, TestNodeHost>> = Vec::with_capacity(TEST_NODE_COUNT);
        for port in TEST_PORT_RANGE_START..(TEST_PORT_RANGE_START + (TEST_NODE_COUNT as u16)) {
            let mut peers: Vec<SocketAddr> = Vec::with_capacity(TEST_NODE_COUNT);
            for port2 in TEST_PORT_RANGE_START..(TEST_PORT_RANGE_START + (TEST_NODE_COUNT as u16)) {
                if port != port2 {
                    peers.push(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port2)));
                }
            }
            let nh = Arc::new(TestNodeHost {
                name: format!("{}", port),
                peers,
                db: Mutex::new(BTreeMap::new())
            });
            //println!("Starting node on 127.0.0.1:{}...", port, nh.db.lock().unwrap().len());
            nodes.push(Node::new(nh.clone(), nh.clone(), SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))).await.unwrap());
        }

        print!("Waiting for all connections to be established...");
        let _ = stdout().flush();
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let mut count = 0;
            for n in nodes.iter() {
                count += n.connection_count().await;
            }
            if count == (TEST_NODE_COUNT * (TEST_NODE_COUNT - 1)) {
                println!(" {} connections up.", count);
                break;
            } else {
                print!(".");
                let _ = stdout().flush();
            }
        }

        println!("Populating maps with data to be synchronized between nodes...");
        let mut all_records = BTreeMap::new();
        for n in nodes.iter_mut() {
            for _ in 0..TEST_STARTING_RECORDS_PER_NODE {
                let mut k = [0_u8; 64];
                let mut v = [0_u8; 32];
                get_random_bytes(&mut k);
                get_random_bytes(&mut v);
                let v: Arc<[u8]> = Arc::from(v);
                all_records.insert(k.clone(), v.clone());
                n.datastore().db.lock().unwrap().insert(k, v);
            }
        }

        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
}
