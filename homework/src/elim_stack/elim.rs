use core::mem::ManuallyDrop;
use core::ops::Deref;
use core::ptr;
use core::sync::atomic::Ordering;
use std::thread;

use crossbeam_epoch::{Atomic, Guard, Owned, Shared};

use super::base::{ELIM_DELAY, ElimStack, Stack, get_random_elim_index};

const EMPTY: usize = 0;
const PUSH_REQ: usize = 1;
const POP_REQ: usize = 2;
const ACKED: usize = 3;

impl<T, S: Stack<T>> Stack<T> for ElimStack<T, S> {
    type PushReq = S::PushReq;

    fn try_push(
        &self,
        req: Owned<Self::PushReq>,
        guard: &Guard,
    ) -> Result<(), Owned<Self::PushReq>> {
        let Err(req) = self.inner.try_push(req, guard) else {
            return Ok(());
        };
        self.try_eliminate_push(req, guard)
    }

    fn try_pop(&self, guard: &Guard) -> Result<Option<T>, ()> {
        if let Ok(result) = self.inner.try_pop(guard) {
            return Ok(result);
        }
        self.try_eliminate_pop(guard)
    }

    fn is_empty(&self, guard: &Guard) -> bool {
        self.inner.is_empty(guard)
    }
}

impl<T, S: Stack<T>> ElimStack<T, S> {
    fn random_slot(&self) -> &Atomic<S::PushReq> {
        &self.slots[get_random_elim_index()]
    }

    fn try_eliminate_push(
        &self,
        req: Owned<S::PushReq>,
        guard: &Guard,
    ) -> Result<(), Owned<S::PushReq>> {
        let slot = self.random_slot();
        let observed = slot.load(Ordering::Acquire, guard);

        let offer = match observed.tag() {
            EMPTY => match slot.post_push_offer(observed, req, guard) {
                PostPushOfferOutcome::Posted(offer) => offer,
                PostPushOfferOutcome::SlotTaken(req) => return Err(req),
            },
            POP_REQ => {
                return match slot.answer_pop_request(observed, req, guard) {
                    AnswerPopRequestOutcome::Answered => Ok(()),
                    AnswerPopRequestOutcome::RequestGone(req) => Err(req),
                };
            }
            _ => return Err(req),
        };

        thread::sleep(ELIM_DELAY);

        match unsafe { slot.withdraw_push_offer(offer, guard) } {
            WithdrawPushOfferOutcome::Withdrawn(req) => Err(req),
            WithdrawPushOfferOutcome::TakenByPopper => {
                unsafe { slot.release_after_pop_took_value(offer, guard) };
                Ok(())
            }
        }
    }

    fn try_eliminate_pop(&self, guard: &Guard) -> Result<Option<T>, ()> {
        let slot = self.random_slot();
        let observed = slot.load(Ordering::Acquire, guard);

        match observed.tag() {
            PUSH_REQ => match slot.accept_push_offer(observed, guard) {
                AcceptPushOfferOutcome::Accepted => Ok(Some(unsafe { take_value(observed) })),
                AcceptPushOfferOutcome::OfferGone => Err(()),
            },
            EMPTY => {
                let pop_request = match slot.post_pop_request(observed, guard) {
                    PostPopRequestOutcome::Posted(pop_request) => pop_request,
                    PostPopRequestOutcome::SlotTaken => return Err(()),
                };

                thread::sleep(ELIM_DELAY);

                match slot.withdraw_pop_request(pop_request, guard) {
                    WithdrawPopRequestOutcome::Withdrawn => Err(()),
                    WithdrawPopRequestOutcome::AnsweredByPusher(node) => {
                        Ok(Some(unsafe { slot.receive_from_pusher(node, guard) }))
                    }
                }
            }
            _ => Err(()),
        }
    }
}

enum PostPushOfferOutcome<'g, R> {
    Posted(Shared<'g, R>),
    SlotTaken(Owned<R>),
}

enum AnswerPopRequestOutcome<R> {
    Answered,
    RequestGone(Owned<R>),
}

enum WithdrawPushOfferOutcome<R> {
    Withdrawn(Owned<R>),
    TakenByPopper,
}

enum AcceptPushOfferOutcome {
    Accepted,
    OfferGone,
}

enum PostPopRequestOutcome<'g, R> {
    Posted(Shared<'g, R>),
    SlotTaken,
}

enum WithdrawPopRequestOutcome<'g, R> {
    Withdrawn,
    AnsweredByPusher(Shared<'g, R>),
}

/// The protocol steps a pusher or popper can perform on an elimination slot.
trait EliminationSlot<R> {
    /// Pusher: `EMPTY` → `node | PUSH_REQ`.
    fn post_push_offer<'g>(
        &self,
        observed: Shared<'_, R>,
        req: Owned<R>,
        guard: &'g Guard,
    ) -> PostPushOfferOutcome<'g, R>;

    /// Pusher: `null | POP_REQ` → `node | ACKED`. When answered, the popper owns `req`.
    fn answer_pop_request(
        &self,
        observed: Shared<'_, R>,
        req: Owned<R>,
        guard: &Guard,
    ) -> AnswerPopRequestOutcome<R>;

    /// Pusher: `node | PUSH_REQ` → `EMPTY`.
    ///
    /// # Safety
    /// `offer` must be the offer this thread posted with `post_push_offer`.
    unsafe fn withdraw_push_offer(
        &self,
        offer: Shared<'_, R>,
        guard: &Guard,
    ) -> WithdrawPushOfferOutcome<R>;

    /// Pusher, after its offer was taken: resets the slot to `EMPTY` and frees the node.
    ///
    /// # Safety
    /// `offer` must be this thread's node, currently stored in the slot with the `ACKED` tag.
    unsafe fn release_after_pop_took_value(&self, offer: Shared<'_, R>, guard: &Guard);

    /// Popper: `node | PUSH_REQ` → `node | ACKED`. When accepted, the popper may take the value.
    fn accept_push_offer(&self, observed: Shared<'_, R>, guard: &Guard) -> AcceptPushOfferOutcome;

    /// Popper: `EMPTY` → `null | POP_REQ`.
    fn post_pop_request<'g>(
        &self,
        observed: Shared<'_, R>,
        guard: &'g Guard,
    ) -> PostPopRequestOutcome<'g, R>;

    /// Popper: `null | POP_REQ` → `EMPTY`.
    fn withdraw_pop_request<'g>(
        &self,
        pop_request: Shared<'_, R>,
        guard: &'g Guard,
    ) -> WithdrawPopRequestOutcome<'g, R>;

    /// Popper, after its request was answered: takes the value, resets the slot to `EMPTY` and
    /// frees the node.
    ///
    /// # Safety
    /// `node` must be the pusher's node, currently stored in the slot with the `ACKED` tag.
    unsafe fn receive_from_pusher<T>(&self, node: Shared<'_, R>, guard: &Guard) -> T
    where
        R: Deref<Target = ManuallyDrop<T>>;
}

impl<R> EliminationSlot<R> for Atomic<R> {
    fn post_push_offer<'g>(
        &self,
        observed: Shared<'_, R>,
        req: Owned<R>,
        guard: &'g Guard,
    ) -> PostPushOfferOutcome<'g, R> {
        match self.compare_exchange(
            observed,
            req.with_tag(PUSH_REQ),
            Ordering::Release,
            Ordering::Relaxed,
            guard,
        ) {
            Ok(offer) => PostPushOfferOutcome::Posted(offer),
            Err(e) => PostPushOfferOutcome::SlotTaken(e.new.with_tag(EMPTY)),
        }
    }

    fn answer_pop_request(
        &self,
        observed: Shared<'_, R>,
        req: Owned<R>,
        guard: &Guard,
    ) -> AnswerPopRequestOutcome<R> {
        match self.compare_exchange(
            observed,
            req.with_tag(ACKED),
            Ordering::Release,
            Ordering::Relaxed,
            guard,
        ) {
            Ok(_) => AnswerPopRequestOutcome::Answered,
            Err(e) => AnswerPopRequestOutcome::RequestGone(e.new.with_tag(EMPTY)),
        }
    }

    unsafe fn withdraw_push_offer(
        &self,
        offer: Shared<'_, R>,
        guard: &Guard,
    ) -> WithdrawPushOfferOutcome<R> {
        match self.compare_exchange(
            offer,
            Shared::null(),
            Ordering::Relaxed,
            Ordering::Acquire,
            guard,
        ) {
            Ok(_) => {
                WithdrawPushOfferOutcome::Withdrawn(unsafe { offer.into_owned() }.with_tag(EMPTY))
            }
            Err(_) => WithdrawPushOfferOutcome::TakenByPopper,
        }
    }

    unsafe fn release_after_pop_took_value(&self, offer: Shared<'_, R>, guard: &Guard) {
        self.store(Shared::null(), Ordering::Relaxed);
        unsafe { guard.defer_destroy(offer) };
    }

    fn accept_push_offer(&self, observed: Shared<'_, R>, guard: &Guard) -> AcceptPushOfferOutcome {
        match self.compare_exchange(
            observed,
            observed.with_tag(ACKED),
            Ordering::Acquire,
            Ordering::Relaxed,
            guard,
        ) {
            Ok(_) => AcceptPushOfferOutcome::Accepted,
            Err(_) => AcceptPushOfferOutcome::OfferGone,
        }
    }

    fn post_pop_request<'g>(
        &self,
        observed: Shared<'_, R>,
        guard: &'g Guard,
    ) -> PostPopRequestOutcome<'g, R> {
        match self.compare_exchange(
            observed,
            Shared::null().with_tag(POP_REQ),
            Ordering::Relaxed,
            Ordering::Relaxed,
            guard,
        ) {
            Ok(pop_request) => PostPopRequestOutcome::Posted(pop_request),
            Err(_) => PostPopRequestOutcome::SlotTaken,
        }
    }

    fn withdraw_pop_request<'g>(
        &self,
        pop_request: Shared<'_, R>,
        guard: &'g Guard,
    ) -> WithdrawPopRequestOutcome<'g, R> {
        match self.compare_exchange(
            pop_request,
            Shared::null(),
            Ordering::Relaxed,
            Ordering::Acquire,
            guard,
        ) {
            Ok(_) => WithdrawPopRequestOutcome::Withdrawn,
            Err(e) => WithdrawPopRequestOutcome::AnsweredByPusher(e.current),
        }
    }

    unsafe fn receive_from_pusher<T>(&self, node: Shared<'_, R>, guard: &Guard) -> T
    where
        R: Deref<Target = ManuallyDrop<T>>,
    {
        let value = unsafe { take_value(node) };
        self.store(Shared::null(), Ordering::Relaxed);
        unsafe { guard.defer_destroy(node) };
        value
    }
}

/// Moves the value out of a push request node without freeing the node.
///
/// # Safety
/// `node` must be non-null and alive, and only one thread may take its value.
unsafe fn take_value<T, R: Deref<Target = ManuallyDrop<T>>>(node: Shared<'_, R>) -> T {
    ManuallyDrop::into_inner(unsafe { ptr::read(node.deref().deref()) })
}
