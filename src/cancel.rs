// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A one-shot notice that the session is ending, for work that can give up.

use std::sync::Arc;
use tokio::sync::watch;

/// A flag a request can wait on, set once when the session is asked to end.
///
/// The agent ends a session by closing stdin and then signalling the process group,
/// and docs/SANDBOX.md gives one second between SIGTERM and SIGKILL.  A request in
/// flight cannot simply be dropped, because a `bash` call that is abandoned between
/// spawning a command and recording it would leave a process group nothing owns.
/// So the waiting part of a request stops waiting instead, and the request answers.
///
/// A `watch` is what makes a late waiter see the same thing an early one does.  A
/// `Notify` would need its registration ordered against the flag by hand.
#[derive(Clone)]
pub struct Cancel(Arc<watch::Sender<bool>>);

impl Default for Cancel {
    fn default() -> Self {
        Self::new()
    }
}

impl Cancel {
    pub fn new() -> Self {
        Self(Arc::new(watch::channel(false).0))
    }

    /// Says the session is ending.  Saying it twice changes nothing.
    pub fn trigger(&self) {
        // `send_replace` records the value with nobody listening, which is the usual
        // case: the signal usually arrives while no request is waiting on it.
        self.0.send_replace(true);
    }

    pub fn triggered(&self) -> bool {
        *self.0.borrow()
    }

    /// Resolves once the session is ending, and at once if it already is.
    pub async fn wait(&self) {
        // Subscribing takes the current value with it, so checking before waiting
        // cannot miss a trigger that happened in between.
        let mut seen = self.0.subscribe();
        loop {
            if *seen.borrow_and_update() {
                return;
            }
            if seen.changed().await.is_err() {
                // The sender is gone, so nothing will ever set the flag.  Ending is
                // the only answer left, and hanging is not an answer.
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn a_wait_before_the_notice_ends_when_it_is_set() {
        let ending = Cancel::new();
        let waiter = ending.clone();
        let wait = tokio::spawn(async move { waiter.wait().await });
        tokio::task::yield_now().await;
        assert!(!ending.triggered());
        ending.trigger();
        wait.await.unwrap();
        assert!(ending.triggered());
    }

    /// The order that matters for a teardown: the notice usually lands first, and a
    /// request that starts waiting afterwards must not sit there for nothing.
    #[tokio::test]
    async fn a_wait_after_the_notice_returns_at_once() {
        let ending = Cancel::new();
        ending.trigger();
        tokio::time::timeout(Duration::from_millis(50), ending.wait())
            .await
            .expect("a triggered flag resolves immediately");
    }

    #[test]
    fn a_clone_sees_the_same_notice() {
        let ending = Cancel::new();
        let other = ending.clone();
        ending.trigger();
        assert!(other.triggered());
        ending.trigger();
        assert!(other.triggered());
    }
}
