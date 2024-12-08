use std::convert::Infallible;
use std::sync::Arc;

use bot::{BotConfig, BotOptions};
use enumset::EnumSet;
use futures::prelude::*;
use tbp::Randomizer;

use crate::bot::Bot;
use crate::data::GameState;
use crate::sync::BotSyncronizer;
use crate::tbp::{BotMessage, FrontendMessage};

mod bot;
mod dag;
mod tbp;
#[macro_use]
pub mod data;
mod map;
pub mod movegen;
mod sync;

use wasm_bindgen::prelude::*;
use futures::channel::mpsc;
use futures::StreamExt;
use crate::tbp::MoveInfo;
use crate::data::Placement;

#[wasm_bindgen]
pub struct Service {
    sender: mpsc::UnboundedSender<String>,
}

#[wasm_bindgen]
impl Service {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Service {
        let (sender, mut receiver) = mpsc::unbounded::<String>();

        wasm_bindgen_futures::spawn_local(async move {
            
            log(&serde_json::to_string(&BotMessage::Info {
                name: "Cold Clear 2",
                version: concat!(env!("CARGO_PKG_VERSION"), " ", env!("GIT_HASH")),
                author: "MinusKelvin",
                features: &[],
            }).unwrap());

            let mut bot = None;
            let mut waiting_on_first_piece = None;
            let config = Arc::new(BotConfig::default());
            while let Some(raw) = receiver.next().await {
                let msg = serde_json::from_str::<FrontendMessage>(&raw).unwrap();
                match msg {
                    FrontendMessage::Start(start) => {
                        if start.hold.is_none() && start.queue.is_empty() {
                            waiting_on_first_piece = Some(start);
                        } else {
                            // bot.start(create_bot(start, config.clone()));
                            bot = Some(create_bot(start, config.clone()));
                            if let Some(bot) = &mut bot {
                                bot.do_work();
                            }
                        }
                    }
                    FrontendMessage::Stop => {
                        bot = None;
                        waiting_on_first_piece = None;
                    }
                    FrontendMessage::Suggest => {
                        if let Some((moves, move_info)) = suggest_bot(&bot) {
                            log(&serde_json::to_string(&BotMessage::Suggestion { moves, move_info }).unwrap());
                        }
                    }
                    FrontendMessage::Play { mv } => {
                        if let Some(bot) = &mut bot {
                            bot.advance(mv);
                            bot.do_work();
                        }
                        puffin::GlobalProfiler::lock().new_frame();
                    }
                    FrontendMessage::NewPiece { piece } => {
                        if let Some(mut start) = waiting_on_first_piece.take() {
                            if let Randomizer::SevenBag { bag_state } = &mut start.randomizer {
                                if bag_state.is_empty() {
                                    *bag_state = EnumSet::all();
                                }
                                bag_state.remove(piece);
                            }
                            start.queue.push(piece);
                            bot = Some(create_bot(start, config.clone()));
                        } else {
                            if let Some(bot) = &mut bot {
                                bot.new_piece(piece);
                                bot.do_work();
                            }
                        }
                    }
                    FrontendMessage::Rules => {
                        log(&serde_json::to_string(&BotMessage::Ready).unwrap());
                    }
                    FrontendMessage::Quit => break,
                    FrontendMessage::Unknown => {}
                }
            }
        });

        Service { sender }
    }

    // 接收來自 JS 的輸入
    pub fn send_input(&self, input: String) {
        self.sender.unbounded_send(input).unwrap();
    }
}

// 用於向主線程輸出結果
#[wasm_bindgen]
extern "C" {
    fn log(s: &str);
}
pub async fn run(
    mut incoming: impl Stream<Item = FrontendMessage> + Unpin,
    mut outgoing: impl Sink<BotMessage, Error = Infallible> + Unpin,
    config: Arc<BotConfig>,
) {
    outgoing
        .send(BotMessage::Info {
            name: "Cold Clear 2",
            version: concat!(env!("CARGO_PKG_VERSION"), " ", env!("GIT_HASH")),
            author: "MinusKelvin",
            features: &[],
        })
        .await
        .unwrap();

    let bot = Arc::new(BotSyncronizer::new());

    spawn_workers(&bot);

    let mut waiting_on_first_piece = None;

    while let Some(msg) = incoming.next().await {
        match msg {
            FrontendMessage::Start(start) => {
                if start.hold.is_none() && start.queue.is_empty() {
                    waiting_on_first_piece = Some(start);
                } else {
                    bot.start(create_bot(start, config.clone()));
                }
            }
            FrontendMessage::Stop => {
                bot.stop();
                waiting_on_first_piece = None;
            }
            FrontendMessage::Suggest => {
                if let Some((moves, move_info)) = bot.suggest() {
                    outgoing
                        .send(BotMessage::Suggestion { moves, move_info })
                        .await
                        .unwrap();
                }
            }
            FrontendMessage::Play { mv } => {
                bot.advance(mv);
                puffin::GlobalProfiler::lock().new_frame();
            }
            FrontendMessage::NewPiece { piece } => {
                if let Some(mut start) = waiting_on_first_piece.take() {
                    if let Randomizer::SevenBag { bag_state } = &mut start.randomizer {
                        if bag_state.is_empty() {
                            *bag_state = EnumSet::all();
                        }
                        bag_state.remove(piece);
                    }
                    start.queue.push(piece);
                    bot.start(create_bot(start, config.clone()));
                } else {
                    bot.new_piece(piece);
                }
            }
            FrontendMessage::Rules => {
                outgoing.send(BotMessage::Ready).await.unwrap();
            }
            FrontendMessage::Quit => break,
            FrontendMessage::Unknown => {}
        }
    }
}

fn create_bot(mut start: tbp::Start, config: Arc<BotConfig>) -> Bot {
    let reserve = start.hold.unwrap_or_else(|| start.queue.remove(0));

    let speculate = matches!(start.randomizer, Randomizer::SevenBag { .. });
    let bag = match start.randomizer {
        Randomizer::Unknown => EnumSet::all(),
        Randomizer::SevenBag { mut bag_state } => {
            for &p in start.queue.iter().rev() {
                if bag_state == EnumSet::all() {
                    bag_state = EnumSet::empty();
                }
                bag_state.insert(p);
            }
            bag_state
        }
    };

    let state = GameState {
        reserve,
        back_to_back: start.back_to_back,
        combo: start.combo.try_into().unwrap_or(255),
        bag,
        board: start.board.into(),
    };

    Bot::new(BotOptions { speculate, config }, state, &start.queue)
}


fn suggest_bot(bot:&Option<Bot>) -> Option<(Vec<Placement>, MoveInfo)> {
    bot.as_ref().map(|bot| {
        let suggestion = bot.suggest();
        let info = MoveInfo {
            nodes: 0,
            nps: 0.0,
            extra: "".to_string(),
        };
        (suggestion, info)
    })
}

fn spawn_workers(bot: &Arc<BotSyncronizer>) {
    for _ in 0..1 {
        let bot = bot.clone();
        std::thread::spawn(move || bot.work_loop());
    }
}
