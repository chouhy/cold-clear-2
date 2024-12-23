use web_time::Instant;
use async_lock::Mutex as AsyncMutex;
use async_lock::RwLock as AsyncRwLock;
use gloo_timers::future::TimeoutFuture;

use parking_lot::{Condvar, Mutex, RwLock};

use crate::bot::{Bot, Statistics};
use crate::data::{Piece, Placement};
use crate::tbp::MoveInfo;

pub struct BotSyncronizer {
    state: Mutex<State>,
    blocker: Condvar,
    bot: RwLock<Option<Bot>>,
}

impl BotSyncronizer {
    pub fn new() -> Self {
        BotSyncronizer {
            state: Mutex::new(State {
                stats: Default::default(),
                last_advance: Instant::now(),
                node_limit: u64::MAX,
                start: Instant::now(),
                nodes_since_start: 0,
            }),
            blocker: Condvar::new(),
            bot: RwLock::new(None),
        }
    }

    pub fn start(&self, initial_state: Bot) {
        let mut state = self.state.lock();
        state.stats = Default::default();
        state.nodes_since_start = 0;
        state.start = Instant::now();
        *self.bot.write() = Some(initial_state);
        self.blocker.notify_all();
    }

    pub fn stop(&self) {
        *self.bot.write() = None;
    }

    pub fn suggest(&self) -> Option<(Vec<Placement>, MoveInfo)> {
        let bot = self.bot.read();
        bot.as_ref().map(|bot| {
            let state = self.state.lock();
            let suggestion = bot.suggest();
            let info = MoveInfo {
                nodes: state.stats.nodes,
                nps: state.stats.nodes as f64 / state.last_advance.elapsed().as_secs_f64(),
                extra: format!(
                    "{:.1}% of selections expanded, overall speed: {:.1} Mnps",
                    state.stats.expansions as f64 / state.stats.selections as f64 * 100.0,
                    state.nodes_since_start as f64 / state.start.elapsed().as_secs_f64() / 1_000_000.0
                )
            };
            (suggestion, info)
        })
    }

    pub fn advance(&self, mv: Placement) {
        let mut state = self.state.lock();
        state.stats = Default::default();
        state.last_advance = Instant::now();
        let mut bot = self.bot.write();
        if let Some(bot) = &mut *bot {
            bot.advance(mv);
        }
        self.blocker.notify_all();
    }

    pub fn new_piece(&self, piece: Piece) {
        let mut bot = self.bot.write();
        if let Some(bot) = &mut *bot {
            bot.new_piece(piece);
        }
        self.blocker.notify_all();
    }

    pub fn work_loop(&self) {
        let mut state = self.state.lock();
        loop {
            if state.stats.nodes > state.node_limit {
                self.blocker.wait(&mut state);
                continue;
            }
            let bot_guard = self.bot.read();
            let bot = match &*bot_guard {
                Some(bot) => bot,
                None => {
                    drop(bot_guard);
                    self.blocker.wait(&mut state);
                    continue;
                }
            };

            drop(state);
            let new_stats = bot.do_work();
            drop(bot_guard);

            state = self.state.lock();
            state.stats.accumulate(new_stats);
            state.nodes_since_start += new_stats.nodes;
        }
    }
}

pub struct BotSyncronizerWASM {
    state: AsyncMutex<State>,
    bot: AsyncRwLock<Option<Bot>>,
}

impl BotSyncronizerWASM {
    pub fn new() -> Self {
        BotSyncronizerWASM {
            state: AsyncMutex::new(State {
                stats: Default::default(),
                last_advance: Instant::now(),
                node_limit: u64::MAX,
                start: Instant::now(),
                nodes_since_start: 0,
            }),
            bot: AsyncRwLock::new(None),
        }
    }

    pub async fn start(&self, initial_state: Bot) {
        let mut state = self.state.lock().await;
        state.stats = Default::default();
        state.nodes_since_start = 0;
        state.start = Instant::now();
        *self.bot.write().await = Some(initial_state);
    }

    pub async fn stop(&self) {
        *self.bot.write().await = None;
    }

    pub async fn suggest(&self) -> Option<(Vec<Placement>, MoveInfo)> {
        let bot = self.bot.read().await;
        if let Some(bot) = bot.as_ref() {
            let state = self.state.lock().await;
            let suggestion = bot.suggest();
            let info = MoveInfo {
                nodes: state.stats.nodes,
                nps: state.stats.nodes as f64 / state.last_advance.elapsed().as_secs_f64(),
                extra: format!(
                    "{:.1}% of selections expanded, overall speed: {:.1} Mnps",
                    state.stats.expansions as f64 / state.stats.selections as f64 * 100.0,
                    state.nodes_since_start as f64 / state.start.elapsed().as_secs_f64() / 1_000_000.0
                )
            };
            Some((suggestion, info))
        }
        else {
            None
        }
    }

    pub async fn advance(&self, mv: Placement) {
        let mut state = self.state.lock().await;
        state.stats = Default::default();
        state.last_advance = Instant::now();
        let mut bot = self.bot.write().await;
        if let Some(bot) = &mut *bot {
            bot.advance(mv);
        }
    }

    pub async fn new_piece(&self, piece: Piece) {
        let mut bot = self.bot.write().await;
        if let Some(bot) = &mut *bot {
            bot.new_piece(piece);
        }
    }

    pub async fn work_loop(&self) {
        let mut cnt = 0;
        loop {
            cnt += 1;
            if (cnt % 100000) == 0 {
                TimeoutFuture::new(1).await;
            }
            let mut state = self.state.lock().await;
            if state.stats.nodes > state.node_limit {
                drop(state);
                continue;
            }
            let bot_guard = self.bot.read().await;
            let bot = match &*bot_guard {
                Some(bot) => bot,
                None => {
                    drop(bot_guard);
                    drop(state);
                    continue;
                }
            };
            let new_stats = bot.do_work();
            state.stats.accumulate(new_stats);
            state.nodes_since_start += new_stats.nodes;
        }
    }
}


#[derive(Copy, Clone, Debug)]
struct State {
    stats: Statistics,
    last_advance: Instant,
    node_limit: u64,
    start: Instant,
    nodes_since_start: u64,
}
