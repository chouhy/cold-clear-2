use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{self};
use std::time::Instant;

use enum_map::EnumMap;
use enumset::EnumSet;
use once_cell::sync::Lazy;
use rand::prelude::*;
use smallvec::SmallVec;

use crate::data::Placement;
use crate::data::{GameState, Piece};
use crate::map::StateMap;
use crate::profile::profiling_frame_end;
use crate::profile::ProfileScope;

pub trait Evaluation: Ord + Copy + Default + std::ops::Add<Self::Reward, Output = Self> {
    type Reward: Copy;

    fn average(of: impl Iterator<Item = Option<Self>>) -> Self;
}

pub struct Dag<E: Evaluation> {
    root: GameState,
    top_layer: Box<Layer<E>>,
    last_advance: Instant,
    new_nodes: AtomicU64,
}

pub struct Selection<'a, E: Evaluation> {
    layers: Vec<&'a Layer<E>>,
    game_state: GameState,
    new_nodes: &'a AtomicU64,
}

pub struct ChildData<E: Evaluation> {
    pub resulting_state: GameState,
    pub mv: Placement,
    pub eval: E,
    pub reward: E::Reward,
}

#[derive(Default)]
struct Layer<E: Evaluation> {
    next_layer: Lazy<Box<Layer<E>>>,
    states: StateMap<Node<E>>,
    piece: Option<Piece>,
}

struct Node<E: Evaluation> {
    parents: SmallVec<[(u64, Placement, Piece); 1]>,
    eval: E,
    children: Option<EnumMap<Piece, Box<[Child<E>]>>>,
    expanding: AtomicBool,
    // we need this info while backpropagating, but we don't have access to the game state then
    bag: EnumSet<Piece>,
    reserve: Piece,
}

#[derive(Clone, Copy, Debug)]
struct Child<E: Evaluation> {
    mv: Placement,
    reward: E::Reward,
    cached_eval: E,
}

impl<E: Evaluation> Dag<E> {
    pub fn new(root: GameState, queue: &[Piece]) -> Self {
        let mut top_layer = Layer::default();
        top_layer.states.insert(
            &root,
            Node {
                parents: SmallVec::new(),
                eval: E::default(),
                children: None,
                expanding: AtomicBool::new(false),
                bag: root.bag,
                reserve: root.reserve,
            },
        );

        let mut layer = &mut top_layer;
        for &piece in queue {
            layer.piece = Some(piece);
            layer = &mut layer.next_layer;
        }

        Dag {
            root,
            top_layer: Box::new(top_layer),
            last_advance: Instant::now(),
            new_nodes: AtomicU64::new(0),
        }
    }

    pub fn advance(&mut self, mv: Placement) {
        let now = Instant::now();
        profiling_frame_end(
            *self.new_nodes.get_mut(),
            now.duration_since(self.last_advance),
        );
        let _scope = ProfileScope::new("advance");
        self.last_advance = now;
        *self.new_nodes.get_mut() = 0;

        let top_layer = std::mem::take(&mut *self.top_layer);
        self.root.advance(
            top_layer.piece.expect("cannot advance without next piece"),
            mv,
        );
        Lazy::force(&top_layer.next_layer);
        self.top_layer = Lazy::into_value(top_layer.next_layer).unwrap();
        let _ = self
            .top_layer
            .states
            .get_or_insert_with(&self.root, || Node {
                parents: SmallVec::new(),
                eval: E::default(),
                children: None,
                expanding: AtomicBool::new(false),
                bag: self.root.bag,
                reserve: self.root.reserve,
            });
    }

    pub fn add_piece(&mut self, piece: Piece) {
        let mut layer = &mut self.top_layer;
        loop {
            if layer.piece.is_none() {
                layer.piece = Some(piece);
                return;
            }
            layer = &mut layer.next_layer;
        }
    }

    pub fn suggest(&self) -> Vec<Placement> {
        let node = self.top_layer.states.get(&self.root).unwrap();
        let children = match &node.children {
            Some(children) => children,
            None => return vec![],
        };

        let mut candidates: Vec<&_> = vec![];
        match self.top_layer.piece {
            Some(next) => {
                candidates.extend(children[next].first());
            }
            None => {
                for piece in self.root.bag {
                    candidates.extend(children[piece].first());
                }
            }
        };
        candidates.sort_by(|a, b| a.cached_eval.partial_cmp(&b.cached_eval).unwrap().reverse());

        candidates.into_iter().map(|c| c.mv).collect()
    }

    pub fn select(&self) -> Option<Selection<E>> {
        let _scope = ProfileScope::new("select");

        let mut layers = vec![&*self.top_layer];
        let mut game_state = self.root;
        loop {
            let &layer = layers.last().unwrap();
            let node = layer
                .states
                .get(&game_state)
                .expect("Link to non-existent node?");

            let children = match &node.children {
                None => {
                    if node.expanding.swap(true, atomic::Ordering::Acquire) {
                        return None;
                    } else {
                        return Some(Selection {
                            layers,
                            game_state,
                            new_nodes: &self.new_nodes,
                        });
                    }
                }
                Some(children) => children,
            };

            // TODO: draw from bag
            let next = layer.piece?;

            if children[next].is_empty() {
                return None;
            }

            let choice = loop {
                let s: f64 = thread_rng().gen();
                let i = -s.log2() as usize;
                if i < children[next].len() {
                    break children[next][i].mv;
                }
            };

            game_state.advance(next, choice);

            layers.push(&layer.next_layer);
        }
    }
}

impl<E: Evaluation> Selection<'_, E> {
    pub fn state(&self) -> (GameState, Option<Piece>) {
        (self.game_state, self.layers.last().unwrap().piece)
    }

    pub fn expand(self, children: EnumMap<Piece, Vec<ChildData<E>>>) {
        let mut layers = self.layers;
        let start_layer = layers.pop().unwrap();
        let next = expand(start_layer, self.new_nodes, self.game_state, children);
        backprop(start_layer, layers, next);
    }
}

fn expand<E: Evaluation>(
    layer: &Layer<E>,
    new_nodes: &AtomicU64,
    parent_state: GameState,
    children: EnumMap<Piece, Vec<ChildData<E>>>,
) -> Vec<(u64, Placement, Piece, u64)> {
    let _scope = ProfileScope::new("expand");

    let mut childs = EnumMap::<_, Vec<_>>::default();

    // We need to acquire the lock on the parent since the backprop routine needs the children
    // lists to exist, and they won't if we're still creating them
    let parent_index = layer.states.index(&parent_state);
    let mut parent = layer.states.get_raw_mut(parent_index).unwrap();

    let next_states = &layer.next_layer.states;
    for (next, child) in children
        .into_iter()
        .flat_map(|(p, children)| children.into_iter().map(move |d| (p, d)))
    {
        let mut node = next_states.get_or_insert_with(&child.resulting_state, || Node {
            parents: SmallVec::new(),
            eval: child.eval,
            children: None,
            expanding: AtomicBool::new(false),
            bag: child.resulting_state.bag,
            reserve: child.resulting_state.reserve,
        });
        node.parents.push((parent_index, child.mv, next));
        childs[next].push(Child {
            mv: child.mv,
            cached_eval: node.eval + child.reward,
            reward: child.reward,
        });
    }
    new_nodes.fetch_add(
        childs.values().map(|l| l.len() as u64).sum(),
        atomic::Ordering::Relaxed,
    );

    for list in childs.values_mut() {
        list.sort_by(|a, b| a.cached_eval.cmp(&b.cached_eval).reverse());
    }

    let mut boxed_slice_childs = EnumMap::default();
    for (k, v) in childs {
        boxed_slice_childs[k] = v.into_boxed_slice();
    }
    parent.children = Some(boxed_slice_childs);

    let mut next = vec![];

    for &(grandparent, mv, n) in &parent.parents {
        next.push((grandparent, mv, n, parent_index));
    }

    next
}

fn backprop<'a, E: Evaluation>(
    mut prev_layer: &'a Layer<E>,
    mut layers: Vec<&'a Layer<E>>,
    mut next: Vec<(u64, Placement, Piece, u64)>,
) {
    let _scope = ProfileScope::new("backprop");

    while let Some(layer) = layers.pop() {
        let mut next_up = vec![];

        for (parent_index, placement, next, child_index) in next {
            let mut parent = layer.states.get_raw_mut(parent_index).unwrap();
            let child_eval = prev_layer.states.get_raw(child_index).unwrap().eval;

            let parent_bag = parent.bag;
            let parent_reserve = parent.reserve;
            let children = parent.children.as_mut().unwrap();
            let list = &mut children[next];

            let mut index = list
                .iter()
                .enumerate()
                .find_map(|(i, c)| (c.mv == placement).then(|| i))
                .unwrap();

            list[index].cached_eval = child_eval + list[index].reward;

            if index > 0 && list[index - 1].cached_eval < list[index].cached_eval {
                // Shift up until the list is in order
                let hole = list[index];
                while index > 0 && list[index - 1].cached_eval < hole.cached_eval {
                    list[index] = list[index - 1];
                    index -= 1;
                }
                list[index] = hole;
            } else if index < list.len() - 1
                && list[index + 1].cached_eval > list[index].cached_eval
            {
                // Shift down until the list is in order
                let hole = list[index];
                while index < list.len() - 1 && list[index + 1].cached_eval > hole.cached_eval {
                    list[index] = list[index + 1];
                    index += 1;
                }
                list[index] = hole;
            }

            if index == 0 {
                let next_possibilities = match layer.piece {
                    Some(p) => EnumSet::only(p),
                    None => parent_bag,
                };

                let best_for = |p: Piece| children[p].first().map(|c| c.cached_eval);

                let eval = E::average(
                    next_possibilities
                        .iter()
                        .map(|p| best_for(p).max(best_for(parent_reserve))),
                );

                if parent.eval != eval {
                    parent.eval = eval;

                    for &(ps, mv, next) in &parent.parents {
                        next_up.push((ps, mv, next, parent_index));
                    }
                }
            }
        }

        next = next_up;
        prev_layer = layer;

        if next.is_empty() {
            break;
        }
    }
}
