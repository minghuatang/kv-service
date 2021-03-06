use std::sync::{Arc, Mutex};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, sync_channel, Receiver};
use std::thread;
use std::time::Duration;

use bincode::{deserialize, serialize};
use rand::Rng;

use self::rpc::Client;
use self::State::{Candidate, Follower, Leader};

pub mod rpc;
mod util;

const HEARBEAT_INTERVAL: u64 = 50;
//const ELECTION_TIMEOUT:u64 = 1000;
const MIN_TIMEOUT: u64 = 200;
const MAX_TIMEOUT: u64 = 400;

const CALLBACK_NUMS : u32 = 4;

pub enum State {
    Follower,
    Candidate,
    Leader,
}

#[derive(Serialize, Deserialize, PartialEq, Clone, Debug)]
pub struct LogEntry {
    pub term: u64,
    pub command: Vec<u8>,
}

pub struct ApplyMsg {
    pub valid: bool,
    pub index: usize,
    pub term: u64,
    pub command: Vec<u8>,
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
pub struct RequestVoteArgs {
    pub term: u64,
    pub candidate_id: i32,
    pub last_log_index: usize,
    pub last_log_term: u64,
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
pub struct RequestVoteReply {
    pub term: u64,
    pub vote_granted: bool,
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
pub struct AppendEntriesArgs {
    pub term: u64,
    pub leader_id: i32,
    pub prev_log_index: usize,
    pub prev_log_term: u64,
    pub entries: Vec<LogEntry>,
    pub leader_commit: usize,
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
pub struct AppendEntriesReply {
    pub term: u64,
    pub success: bool,
    pub first_index: usize,  // first index in conflict term
}

pub struct Raft {
    peers: Vec<Client>,     // id of all peers
    pub me: i32,        // this peer's id, index of peers vec
    pub state: State,   // current state of this peer
    apply_ch: SyncSender<ApplyMsg>,

    pub current_term: u64,  // latest term server has seen (initialized to 0 on first boot, increases monotonically)
    vote_for: i32,          // candidateId that received vote in current term (or -1 if none)
    commit_index: usize,      // index of highest log entry known to be committed (initialized to 0, increases monotonically)
    log: Vec<LogEntry>,     // log entries (first index is 1)

    pub next_index: Vec<usize>, // for each server, index of the next log entry to send to that server (initialized to leader last log index + 1)
    pub match_index: Vec<usize>, // for each server, index of highest log entry known to be replicated on server (initialized to 0, increases monotonically)

    election_timer: SyncSender<()>,

    pub voted_cnt: i32, // voted count during a election

    reply_sender : Vec<SyncSender<(Vec<u8>, bool)>>,
}

impl Raft {
    // create a new raft node.
    pub fn new(
        id: i32,
        addr : &Vec<String>,
        apply_ch: &SyncSender<ApplyMsg>,
    ) -> (Arc<Mutex<Raft>>, Client, Vec<SyncSender<(Vec<u8>, bool)>>, Vec<Receiver<Vec<u8>>>) {
        let (peers, mut reply_sendv, mut req_recvv) = Self::create_server(addr, id);
        let put_reply = reply_sendv.pop().unwrap();
        let get_reply = reply_sendv.pop().unwrap();

        let put_req = req_recvv.pop().unwrap();
        let get_req = req_recvv.pop().unwrap();
        let client = peers[id as usize].clone();

//        let (ns, nr) = mpsc::sync_channel(1);
//        let (ms, mr) = mpsc::sync_channel(1);
        let (ts, tr) = mpsc::sync_channel(1);
        let mut r = Raft {
            peers: peers,
            me: id,
            state: Follower,
            apply_ch: apply_ch.clone(),
            current_term: 0,
            vote_for: -1,
            commit_index: 0,
            log: vec![LogEntry {
                term: 0,
                command: Vec::new(),
            }],
            next_index: Vec::new(),
            match_index: Vec::new(),
            voted_cnt: 0,
            election_timer: ts,
            reply_sender : reply_sendv,
        };
        r.next_index.resize(r.peers.len(),0);
        r.match_index.resize(r.peers.len(),0);
        let ret = Arc::new(Mutex::new(r));

        Self::register_callback(&ret, req_recvv);

        let arc_r = ret.clone();
        // election daemon
        thread::spawn(move || { Self::tick_election(tr, arc_r) });
        (ret, client, vec![get_reply, put_reply], vec![get_req, put_req])
    }

    // start to execute a command.
    // if this is not leader, return false immediately
    // return values: command index in the log, current term, is_leader
    pub fn start(r: Arc<Mutex<Raft>>, command: &Vec<u8>) -> (usize, u64, bool) {
        let mut rf = r.lock().unwrap();
//        println!("{} starts",rf.me);
        let (index, term, mut is_leader) = (rf.log.len(), rf.current_term, false);

        if let Leader = rf.state {
            is_leader = true;
            let (me,current_term) = (rf.me as usize,rf.current_term);
            rf.match_index[me] = index;
            rf.log.push(LogEntry{term:current_term, command:command.clone()});
//            println!("{} is leader, return", rf.me);
        }
        (index,term,is_leader)
    }

    // implement AppendEntries RPC.
    pub fn append_entries(r: &Arc<Mutex<Raft>>, args: &mut AppendEntriesArgs) -> AppendEntriesReply {
        let mut rf = r.lock().unwrap();
        // println!("run append_entries in id {}", rf.me);

        let mut reply = AppendEntriesReply {
            success: false, // success only if leader is valid and prev entry matched
            term: rf.current_term,
            first_index: args.prev_log_index+1,
        };

        if args.term < rf.current_term { // expired leader
            return reply;
        }
        rf.election_timer.send(()).unwrap();   // valid leader, reset election timeout

        if args.term > rf.current_term{
            rf.current_term = args.term;
            reply.term = rf.current_term;
        }

        rf.state = Follower;

        let mut last = 0; // last entry matched
        let prev_entry_match = args.prev_log_index<rf.log.len() && rf.log[args.prev_log_index].term == args.prev_log_term;

        if prev_entry_match {
            last = args.prev_log_index;
            reply.success = true;
            if args.entries.len()>0 {
//                println!("{} get entry from {}",rf.me,args.leader_id);
                // delete conflict entries
                last+=args.entries.len();
                rf.log.truncate(args.prev_log_index+1);
                rf.log.append(&mut args.entries);
            }
        } else {
            // to find first index in conflict term
            let mut index;
            if args.prev_log_index < rf.log.len() {
                // search the first entry in conflict term
                index = args.prev_log_index;
                let term = rf.log[index].term;
                while term == rf.log[index-1].term && index > 1 {
                    index -= 1
                }
            } else {
                index = rf.log.len();
            }

            reply.first_index = index;
        }

        // try commit
        if args.leader_commit > rf.commit_index && prev_entry_match {
            let commit_index = std::cmp::min(args.leader_commit, last);
            if rf.commit_index<commit_index {
                let r1 = r.clone();
                let leader_commit = args.leader_commit;
                thread::spawn(move || { Self::commit_to_index(r1, std::cmp::min(leader_commit, last)) });
            }
//            Self::commit_to_index(r1,std::cmp::min(args.leader_commit, last));
        }

        reply
    }

    // implement RequestVote RPC.
    pub fn request_vote(r: &Arc<Mutex<Raft>>, args: &RequestVoteArgs) -> RequestVoteReply {
        let mut rf = r.lock().unwrap();
        // println!("run request_vote in id {}", rf.me);
        let mut reply = RequestVoteReply { term: rf.current_term, vote_granted: false };
        if args.term < rf.current_term {
            // reject because candidate expired
            println!("{} refuse for term to {}", rf.me, args.candidate_id);
            return reply;
        }

        // candidate's log entry inspect
        let last_index = rf.last_index();
        let up_to_date = if rf.log[last_index].term < args.last_log_term {
            true
        } else if rf.log[last_index].term < args.last_log_term {
            false
        } else {
            args.last_log_index >= last_index
        };

        if !up_to_date {
            println!("{} refuse for log entry not up to date to {}", rf.me, args.candidate_id);
            return reply;
        }

        //if candidate's term is greater, grant
        if args.term > rf.current_term {
            rf.vote_for = -1;
            rf.current_term = args.term;
            reply.term = rf.current_term;
        }

        if rf.vote_for == -1 {
            rf.election_timer.send(()).unwrap();
            rf.state = Follower;
            reply.vote_granted = true;
            println!("grant server {} to {} in term {}", rf.me, args.candidate_id, args.term);
            rf.vote_for = args.candidate_id;
        }
        if reply.vote_granted == false {
            println!("{} refuse {} because already voted for {}\n",rf.me, args.candidate_id, rf.vote_for);
        }
        reply
    }

    // get current state of Raft.
    pub fn get_state(r: Arc<Mutex<Raft>>) -> (u64, bool) {
        let raft = r.lock().unwrap();
        println!("get {} state",raft.me);
        let term = raft.current_term;
        let is_leader = match raft.state {
            Leader => true,
            _ => false,
        };
        (term, is_leader)
    }

    // leader election.
    fn campaign(r: Arc<Mutex<Raft>>) {
        let mut rf = r.lock().unwrap();
        rf.voted_cnt = 0;
        rf.vote_for = rf.me;
        rf.state = Candidate;
        rf.current_term += 1;
        let last_index = rf.last_index();
        let last_term = rf.log[last_index].term;
//        let args = RequestVoteArgs { term: rf.current_term, candidate_id: rf.me, last_log_index: last_index, last_log_term: last_term };

        // send request to every peer
        for i in 0..rf.peers.len() {
            if i as i32 == rf.me {
                continue;
            }
            let r1 = r.clone();
            let client = rf.peers[i].clone();
            let args = RequestVoteArgs { term: rf.current_term, candidate_id: rf.me, last_log_index: last_index, last_log_term: last_term };
            // send requests
            thread::spawn(move || {
                match Self::send_request_vote(&client, args) {
                    // got reply
                    Ok(reply) => {
                        let mut rf1 = r1.lock().unwrap();
//                        println!("{} get reply from {}", rf1.me, i);
                        if let Candidate = rf1.state {
                            //got voted
                            if reply.vote_granted && reply.term==rf1.current_term {
                                rf1.voted_cnt += 1;
                                println!("{} get voted {} times", rf1.me,rf1.voted_cnt);
                                // win
                                if rf1.voted_cnt as usize == rf1.peers.len() / 2 {
                                    rf1.state = Leader;
                                    println!("{} is leader of term {}",rf1.me,rf1.current_term);
                                    // initiate leader state
                                    for i in 0..rf1.peers.len() {
                                        rf1.match_index[i] = 0;
                                        rf1.next_index[i] = rf1.log.len();
                                    }
                                    let me = rf1.me as usize;
                                    rf1.match_index[me] = rf1.last_index();
                                    // tick heart beat
                                    let r1 = r1.clone();
                                    thread::spawn(move || {
                                        Self::tick_heartbeat(r1);
                                    });
                                }
                            } else {
                                println!("{} didnt get voted from {}", rf1.me, i);
                                if reply.term > rf1.current_term {
                                    rf1.state = Follower;
                                    rf1.election_timer.send(()).unwrap();  // reset timer
                                    rf1.current_term = reply.term;
                                }
                            }
                        }
                    }
                    Err(err) => {
                         println!("no reply while send vote request to {}, error:{:?}", i, err);
                    }
                }
            });
        }
    }

    // call AppendEntries RPC of one peer.
    fn send_append_entries(client:&Client, args: AppendEntriesArgs) -> Result<AppendEntriesReply, &'static str> {
        let req = serialize(&args).unwrap();
        let (reply, success) = client.call(String::from("Raft.AppendEntries"),req);
        if success {
            let reply: AppendEntriesReply = deserialize(&reply).unwrap();
            return Ok(reply);
        }
        Err("get append entries rpc reply error")
    }

    // call RequestVote RPC of one peer.
    fn send_request_vote(client: &Client, args: RequestVoteArgs) -> Result<RequestVoteReply, &'static str> {
//        let reply = RequestVoteReply{term:0, vote_granted:false};
        let req = serialize(&args).unwrap();
        let (reply, success) = client.call(String::from("Raft.RequestVote"), req);
        if success {
            let reply: RequestVoteReply = deserialize(&reply).unwrap();
            return Ok(reply);
        }
        Err("get request vote rpc reply error")
    }

    // send heartbeat to followers within a given time interval.
    // only call by leader.
    // heartbeats include append_entries rpc
    fn tick_heartbeat(r: Arc<Mutex<Raft>>) {
        loop {
            {
//                 println!("broadcast before lock");
                let rf = r.lock().unwrap();
                println!("leader {} broadcast", rf.me);
                if let Leader = rf.state {
                    rf.election_timer.send(()).unwrap();  //reset timer so leader won't start another election
                    // broadcast
                    for i in 0..rf.peers.len() {
                        if i == rf.me as usize {
                            continue;
                        }

                        // avoid out of index range
                        let pre_index = std::cmp::min(rf.next_index[i]-1,rf.last_index());
                        let pre_term = rf.log[pre_index].term;

                        let mut args = AppendEntriesArgs{
                            leader_id:rf.me,
                            term:rf.current_term,
                            entries:vec![],
                            leader_commit:rf.commit_index,
                            prev_log_term:pre_term,
                            prev_log_index:pre_index,
                        };

                        // append multiple entries
                        let mut next = rf.next_index[i];
                        let mut cnt = 0;
                        while next < rf.log.len() && cnt<10 {
//                            println!("leader {} in term {} append entires at index {} for {}",rf.me,rf.current_term, next, i);
                            args.entries.push(rf.log[next].clone());
                            next += 1;
                            cnt+=1;
                        }

                        // start send append rpc to each server
                        let r1 = r.clone();
                        let client = rf.peers[i].clone();
                        thread::spawn(move||{
                            let num_entries = args.entries.len();
                            match Self::send_append_entries(&client, args) {
                                Ok(reply) => {
                                    let mut rf1 = r1.lock().unwrap();
                                    if let Leader = rf1.state {
                                        if reply.success {
                                            // update index state and try to commit
                                            rf1.match_index[i] = pre_index+num_entries;
                                            rf1.next_index[i] += num_entries;
//                                            println!("next entry for {} is {}",i,rf1.next_index[i]);
                                            let r2 = r1.clone();
                                            // try to commit new appended entries
                                            thread::spawn(move||{Self::leader_commit(r2)});
                                        } else {
                                            if reply.term > rf1.current_term { // leader expired
                                                rf1.state = Follower;
                                                rf1.election_timer.send(()).unwrap();
                                                rf1.current_term = reply.term;
                                            } else { // update next entry according to reply
                                                rf1.next_index[i] = reply.first_index;
                                            }
                                        }
                                    }
                                }
                                Err(err) => {
                                    println!("no reply while send append request to {}, error:{:?}", i, err);
                                }
                            }
                        });
                    }
                } else {
                    return;
                }
            } // unlock during sleep
            thread::sleep(Duration::from_millis(HEARBEAT_INTERVAL));
        }
    }

    // start election after timeout.
    fn tick_election(receiver: Receiver<()>, r: Arc<Mutex<Raft>>) {
        loop {
            match receiver.recv_timeout(Self::random_timeout(MIN_TIMEOUT, MAX_TIMEOUT)) {
                Ok(_) => continue,
                Err(RecvTimeoutError::Timeout) => {
                    {
                        let rf = r.lock().unwrap();
                        println!("{} timeout, start election!",rf.me);
                    }
                    let r1 = r.clone();
                    thread::spawn(move || { Self::campaign(r1) });
                },
                Err(_) => {
                    println!("election timer error");
                },
            };
        }
    }

    // leader try to commit
    fn leader_commit(r: Arc<Mutex<Raft>>) {
        let rf = r.lock().unwrap();
        match rf.state {
            Leader => {},
            _ => return,    // not leader, return
        };
        let mut match_state = rf.match_index.clone();
        match_state.sort();

        let majority = match_state[match_state.len()/2];  //match index of majority

        // only commit current term's entry
        if rf.log[majority].term == rf.current_term && rf.commit_index<majority {
            let r1 = r.clone();
            thread::spawn(move||{
                Self::commit_to_index(r1,majority);
            });
        }
    }

    // commit index and all indices preceding index
    fn commit_to_index(r: Arc<Mutex<Raft>>,index: usize) {
//        println!("commit lock");
        let mut rf = r.lock().unwrap();
//        println!("{} commit start\n",rf.me);
        if rf.commit_index < index {
            for i in rf.commit_index+1..index+1 {
                if i<rf.log.len() {
                    rf.commit_index = i;
                    let msg = ApplyMsg{
                        command:rf.log[i].command.clone(),
                        valid:true,
                        index:i,
                        term:rf.log[i].term,
                    };
                     rf.apply_ch.send(msg).unwrap();
                }
            }
        }
    }

    fn last_index(&self) -> usize {
        self.log.len() - 1
    }

    fn random_timeout(min: u64, max: u64) -> Duration {
        let timeout = rand::thread_rng().gen_range(min, max);
        Duration::from_millis(timeout)
    }


    fn register_callback(r: &Arc<Mutex<Raft>>,  mut req_receiver : Vec<Receiver<Vec<u8>>>) {
        let rr = r.clone();
        let req_receiver0 = req_receiver.remove(0);
        thread::spawn(move || { //RequestVote
            loop {
                let args = req_receiver0.recv().unwrap();
                
                let req : RequestVoteArgs = deserialize(&args[..]).unwrap();
                let reply = Self::request_vote(&rr, &req);
//                let vote_granted = reply.vote_granted;
                let reply = serialize(&reply).unwrap();

                let r1 = rr.lock().unwrap();
                r1.reply_sender[0].send((reply, true)).unwrap();
            }
        });
        let rr = r.clone();
        let req_receiver1 = req_receiver.remove(0);
        thread::spawn(move || { //AppendEntries
            loop {
                let args = req_receiver1.recv().unwrap();
                
                let mut req : AppendEntriesArgs = deserialize(&args[..]).unwrap();
                let reply = Self::append_entries(&rr, &mut req);
                let reply = serialize(&reply).unwrap();

                let r1 = rr.lock().unwrap();
                r1.reply_sender[1].send((reply, true)).unwrap();
            }
        });
    }

    fn create_server(addrs : &Vec<String>, cur_id : i32) -> (Vec<Client>, Vec<SyncSender<(Vec<u8>, bool)>>, Vec<Receiver<Vec<u8>>>) {
        let mut req_sendv = Vec::new();
        let mut reply_sendv = Vec::new();
        let mut req_recvv = Vec::new();
        let mut reply_recvv = Vec::new();
        
        for _i in 0..CALLBACK_NUMS {
            let (req_send, req_recv) = sync_channel(1);
            let (reply_send, reply_recv) = sync_channel(1);

            req_sendv.push(req_send);
            reply_sendv.push(reply_send);
            req_recvv.push(req_recv);
            reply_recvv.push(reply_recv);
        }

        let rn1 = rpc::make_network(addrs[cur_id as usize].clone(), req_sendv, reply_recvv);

        println!("creating server {}", cur_id);
        thread::sleep(Duration::from_secs(1));

        let mut clients = Vec::new();
        for j in 0..addrs.len() {
            // if cur_id as usize == j {
            //     clients.push(Client::new());
            // } else {
                let client = rpc::make_end(&rn1, format!("client{}to{}", cur_id, j), addrs[j].clone());
                clients.push(client);
            // }
        }

        (clients, reply_sendv, req_recvv)
    }
}

#[cfg(test)]
mod tests {
    use std::thread;
    use super::*;

    #[test]
    fn raft_test() {
        let server_num = 3;
        let mut base_port = 8810;
        let mut addrs = Vec::new();
        for i in (0..server_num) {
            addrs.push(format!("127.0.0.1:{}", base_port));
            base_port += 1;
        }
        let aaddrs = Arc::new(addrs);

        for i in (0..server_num) {
            let aaddrs1 = aaddrs.clone();
            thread::spawn(move || {
                let (sx, rx) = sync_channel(1);
                let raft = Raft::new(i, &aaddrs1, &sx);
                thread::sleep(Duration::from_secs(60));
            });
        }

        thread::sleep(Duration::from_secs(60));
    }
}
