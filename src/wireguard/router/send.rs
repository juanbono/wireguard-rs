use super::queue::{Job, Queue};
use super::KeyPair;
use super::types::Callbacks;
use super::peer::Peer;
use super::{REJECT_AFTER_MESSAGES, SIZE_TAG};
use super::messages::{TransportHeader, TYPE_TRANSPORT};

use super::super::{tun, udp, Endpoint};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305};
use zerocopy::{AsBytes, LayoutVerified};
use spin::Mutex;

struct Inner<E: Endpoint, C: Callbacks, T: tun::Writer, B: udp::Writer<E>> {
    ready: AtomicBool,
    buffer: Mutex<Vec<u8>>,
    counter: u64,
    keypair: Arc<KeyPair>,
    peer: Peer<E, C, T, B>,
}

pub struct SendJob<E: Endpoint, C: Callbacks, T: tun::Writer, B: udp::Writer<E>>  {
    inner: Arc<Inner<E, C, T, B>>,
}

impl <E: Endpoint, C: Callbacks, T: tun::Writer, B: udp::Writer<E>> SendJob<E, C, T, B> {
    pub fn new(
        buffer: Vec<u8>,
        counter: u64,
        keypair: Arc<KeyPair>,
        peer: Peer<E, C, T, B>
    ) -> Option<SendJob<E, C, T, B>> {
        // create job
        let inner = Arc::new(Inner{
            buffer: Mutex::new(buffer),
            counter,
            keypair,
            peer,
            ready: AtomicBool::new(false)
        });

        // attempt to add to queue
        if peer.outbound.push(SendJob{ inner: inner.clone()}) {
            Some(SendJob{inner})
        } else {
            None
        }
    }
}

impl <E: Endpoint, C: Callbacks, T: tun::Writer, B: udp::Writer<E>> Job for SendJob<E, C, T, B> {
    fn queue(&self) -> &Queue<Self> {
        &self.inner.peer.outbound
    }

    fn is_ready(&self) -> bool {
        self.inner.ready.load(Ordering::Acquire)
    }

    fn parallel_work(&self) {
        debug_assert_eq!(
            self.is_ready(),
            false,
            "doing parallel work on completed job"
        );
        log::trace!("processing parallel send job");

        // encrypt body
        {
            // make space for the tag
            let job = &*self.inner;
            let mut msg = job.buffer.lock();
            msg.extend([0u8; SIZE_TAG].iter());

            // cast to header (should never fail)
            let (mut header, packet): (LayoutVerified<&mut [u8], TransportHeader>, &mut [u8]) =
                LayoutVerified::new_from_prefix(&mut msg[..])
                    .expect("earlier code should ensure that there is ample space");

            // set header fields
            debug_assert!(
                job.counter < REJECT_AFTER_MESSAGES,
                "should be checked when assigning counters"
            );
            header.f_type.set(TYPE_TRANSPORT);
            header.f_receiver.set(job.keypair.send.id);
            header.f_counter.set(job.counter);

            // create a nonce object
            let mut nonce = [0u8; 12];
            debug_assert_eq!(nonce.len(), CHACHA20_POLY1305.nonce_len());
            nonce[4..].copy_from_slice(header.f_counter.as_bytes());
            let nonce = Nonce::assume_unique_for_key(nonce);

            // do the weird ring AEAD dance
            let key = LessSafeKey::new(
                UnboundKey::new(&CHACHA20_POLY1305, &job.keypair.send.key[..]).unwrap(),
            );

            // encrypt contents of transport message in-place
            let end = packet.len() - SIZE_TAG;
            let tag = key
                .seal_in_place_separate_tag(nonce, Aad::empty(), &mut packet[..end])
                .unwrap();

            // append tag
            packet[end..].copy_from_slice(tag.as_ref());
        }

        // mark ready
        self.inner.ready.store(true, Ordering::Release);
    }

    fn sequential_work(self) {
        debug_assert_eq!(
            self.is_ready(),
            true,
            "doing sequential work 
            on an incomplete job"
        );
        log::trace!("processing sequential send job");

        // send to peer
        let job = &self.inner;
        let msg = job.buffer.lock();
        let xmit = job.peer.send(&msg[..]).is_ok();

        // trigger callback (for timers)
        C::send(
            &job.peer.opaque,
            msg.len(),
            xmit,
            &job.keypair,
            job.counter,
        );        
    }
}