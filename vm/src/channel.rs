// Copyright 2022 The Goscript Authors. All rights reserved.
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

use super::instruction::*;
use super::value::*;
use futures_lite::future;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

#[derive(Clone, Debug)]
pub enum RendezvousState {
    NotReady,
    Ready,
    InPlace(GosValue),
    Closed,
}

#[derive(Clone, Debug)]
pub enum Channel {
    Bounded(
        async_channel::Sender<GosValue>,
        async_channel::Receiver<GosValue>,
    ),
    // Cloning Channel needs to return the same channel, hence the Rc
    Rendezvous(Rc<RefCell<RendezvousState>>),
}

impl Channel {
    pub fn new(cap: usize) -> Channel {
        if cap == 0 {
            Channel::Rendezvous(Rc::new(RefCell::new(RendezvousState::NotReady)))
        } else {
            let (s, r) = async_channel::bounded(cap);
            Channel::Bounded(s, r)
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        match self {
            Channel::Bounded(s, _) => s.len(),
            Channel::Rendezvous(_) => 0,
        }
    }

    #[inline]
    pub fn cap(&self) -> usize {
        match self {
            Channel::Bounded(s, _) => s.capacity().unwrap(),
            Channel::Rendezvous(_) => 0,
        }
    }

    #[inline]
    pub fn close(&self) {
        match self {
            Channel::Bounded(s, _) => {
                s.close();
            }
            Channel::Rendezvous(state) => *state.borrow_mut() = RendezvousState::Closed,
        }
    }

    pub fn try_send(&self, v: GosValue) -> Result<(), async_channel::TrySendError<GosValue>> {
        match self {
            Channel::Bounded(s, _) => s.try_send(v),
            Channel::Rendezvous(state) => {
                let mut state_ref = state.borrow_mut();
                let s: &RendezvousState = &state_ref;
                match s {
                    RendezvousState::NotReady => Err(async_channel::TrySendError::Full(v)),
                    RendezvousState::Ready => {
                        *state_ref = RendezvousState::InPlace(v);
                        Ok(())
                    }
                    RendezvousState::InPlace(_) => Err(async_channel::TrySendError::Full(v)),
                    RendezvousState::Closed => Err(async_channel::TrySendError::Closed(v)),
                }
            }
        }
    }

    pub fn try_recv(&self) -> Result<GosValue, async_channel::TryRecvError> {
        match self {
            Channel::Bounded(_, r) => r.try_recv(),
            Channel::Rendezvous(state) => {
                let mut state_ref = state.borrow_mut();
                let s: &RendezvousState = &state_ref;
                match s {
                    RendezvousState::NotReady => {
                        *state_ref = RendezvousState::Ready;
                        Err(async_channel::TryRecvError::Empty)
                    }
                    RendezvousState::Ready => Err(async_channel::TryRecvError::Empty),
                    RendezvousState::InPlace(_) => {
                        drop(state_ref);
                        if let RendezvousState::InPlace(v) =
                            state.replace(RendezvousState::NotReady)
                        {
                            Ok(v)
                        } else {
                            unreachable!()
                        }
                    }
                    RendezvousState::Closed => Err(async_channel::TryRecvError::Closed),
                }
            }
        }
    }

    pub async fn send(&self, v: &GosValue) -> RuntimeResult<()> {
        let mut val = Some(v.clone());
        loop {
            match self.try_send(val.take().unwrap()) {
                Ok(()) => return Ok(()),
                Err(e) => match e {
                    async_channel::TrySendError::Full(v) => {
                        val = Some(v);
                        future::yield_now().await;
                    }
                    async_channel::TrySendError::Closed(_) => {
                        return Err("channel closed!".to_owned());
                    }
                },
            }
        }
    }

    pub async fn recv(&self) -> Option<GosValue> {
        //dbg!(self);
        loop {
            match self.try_recv() {
                Ok(v) => return Some(v),
                Err(e) => match e {
                    async_channel::TryRecvError::Empty => {
                        future::yield_now().await;
                    }
                    async_channel::TryRecvError::Closed => return None,
                },
            }
        }
    }
}

pub enum SelectCommType {
    Send(GosValue),
    Recv(ValueType, OpIndex),
}

pub struct SelectComm {
    pub typ: SelectCommType,
    pub chan: GosValue,
    pub offset: OpIndex,
}

pub struct Selector {
    pub comms: Vec<SelectComm>,
    pub default_offset: Option<OpIndex>,
    // use a fake random to avoid dependencies
    pseudo_rand: Cell<usize>,
}

impl Selector {
    pub fn new(comms: Vec<SelectComm>, default_offset: Option<OpIndex>) -> Selector {
        Selector {
            comms,
            default_offset,
            pseudo_rand: Cell::new(0),
        }
    }

    pub async fn select(&self) -> RuntimeResult<(usize, Option<GosValue>)> {
        let count = self.comms.len();
        let rand_start = self.pseudo_rand.get();
        self.pseudo_rand.set(rand_start + 1);
        loop {
            for i in 0..count {
                let index = (i + rand_start) % count;
                let entry = &self.comms[index];
                match &entry.typ {
                    SelectCommType::Send(val) => {
                        match entry.chan.as_some_channel()?.chan.try_send(val.clone()) {
                            Ok(_) => return Ok((index, None)),
                            Err(e) => match e {
                                async_channel::TrySendError::Full(_) => {}
                                async_channel::TrySendError::Closed(_) => {
                                    return Err("channel closed!".to_owned());
                                }
                            },
                        }
                    }
                    SelectCommType::Recv(_, _) => {
                        match entry.chan.as_some_channel()?.chan.try_recv() {
                            Ok(v) => return Ok((index, Some(v))),
                            Err(e) => match e {
                                async_channel::TryRecvError::Empty => {}
                                async_channel::TryRecvError::Closed => return Ok((index, None)),
                            },
                        }
                    }
                }
            }

            if let Some(_) = self.default_offset {
                return Ok((self.comms.len(), None));
            }
            future::yield_now().await;
        }
    }
}
