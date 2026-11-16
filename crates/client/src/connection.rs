//! The WebSocket's life as a pure state machine: it says what to do, the page does it (timers,
//! sockets, navigation).

use jarvis_protocol::{CLOSE_SIGNED_OUT, ClientMsg};

use crate::Chat;

const FIRST_DELAY_MS: u32 = 1_000;
const MAX_DELAY_MS: u32 = 30_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Connecting,
    Open,
    /// Waiting to reconnect after `attempt` failures in a row.
    Retrying {
        attempt: u32,
    },
    /// The server refused the session: no retry, the page goes to sign in.
    SignedOut,
}

/// What the page must do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Reconnect after this many milliseconds.
    Retry(u32),
    /// Reconnect right now.
    RetryNow,
    GoToSignIn,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
    state: State,
    failures: u32,
}

impl Default for Connection {
    fn default() -> Self {
        Self {
            state: State::Connecting,
            failures: 0,
        }
    }
}

impl Connection {
    pub fn state(&self) -> State {
        self.state
    }

    /// The socket opened: the hello to send, naming what the chat already has.
    pub fn on_open(&mut self, chat: &Chat) -> ClientMsg {
        self.state = State::Open;
        ClientMsg::Hello {
            last_seq: chat.last_seq(),
            log_id: chat.log_id().map(str::to_owned),
        }
    }

    /// The server's hello arrived: only now is the connection healthy, so the backoff starts
    /// over. Opening alone proves nothing behind a proxy or a backend that closes at once.
    pub fn on_hello(&mut self) {
        self.failures = 0;
    }

    /// The socket closed, or never opened, with this close code.
    pub fn on_close(&mut self, code: u16) -> Action {
        if code == CLOSE_SIGNED_OUT || self.state == State::SignedOut {
            self.state = State::SignedOut;
            return Action::GoToSignIn;
        }
        self.failures += 1;
        self.state = State::Retrying {
            attempt: self.failures,
        };
        let doubling = (self.failures - 1).min(5);
        Action::Retry((FIRST_DELAY_MS << doubling).min(MAX_DELAY_MS))
    }

    /// The retry timer fired: true when the page should open a socket now, false when a retry
    /// already started (see [`Connection::on_visible`]) or the session is gone.
    pub fn on_retry(&mut self) -> bool {
        let waiting = matches!(self.state, State::Retrying { .. });
        if waiting {
            self.state = State::Connecting;
        }
        waiting
    }

    /// The page became visible: a waiting retry happens now instead.
    pub fn on_visible(&mut self) -> Option<Action> {
        if matches!(self.state, State::Retrying { .. }) {
            self.state = State::Connecting;
            // The phone just woke: the old failures say nothing about the network now.
            self.failures = 0;
            Some(Action::RetryNow)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jarvis_protocol::ServerMsg;

    #[test]
    fn open_sends_hello_with_what_the_chat_has() {
        let mut chat = Chat::default();
        let mut connection = Connection::default();
        assert_eq!(
            connection.on_open(&chat),
            ClientMsg::Hello {
                last_seq: 0,
                log_id: None
            }
        );
        chat.apply(&ServerMsg::Hello {
            log_id: "a".into(),
            last_seq: 0,
        });
        assert_eq!(connection.state(), State::Open);
        assert_eq!(
            connection.on_open(&chat),
            ClientMsg::Hello {
                last_seq: 0,
                log_id: Some("a".into())
            }
        );
    }

    #[test]
    fn closes_with_4401_navigates_to_sign_in() {
        let mut connection = Connection::default();
        assert_eq!(connection.on_close(4401), Action::GoToSignIn);
        assert_eq!(connection.state(), State::SignedOut);
        // No retry afterwards, whatever the code.
        assert_eq!(connection.on_close(1006), Action::GoToSignIn);
        assert_eq!(connection.on_visible(), None);
    }

    #[test]
    fn backoff_doubles_to_thirty_seconds() {
        let mut connection = Connection::default();
        let mut delays = Vec::new();
        for _ in 0..8 {
            let Action::Retry(delay) = connection.on_close(1006) else {
                panic!("expected a retry");
            };
            delays.push(delay);
            assert!(connection.on_retry());
            assert_eq!(connection.state(), State::Connecting);
        }
        assert_eq!(
            delays,
            [1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000, 30_000]
        );
        // A server that said hello starts the backoff over.
        connection.on_open(&Chat::default());
        connection.on_hello();
        assert_eq!(connection.on_close(1006), Action::Retry(1_000));
        assert_eq!(connection.state(), State::Retrying { attempt: 1 });
    }

    #[test]
    fn visible_retries_now_when_waiting() {
        let mut connection = Connection::default();
        assert_eq!(connection.on_visible(), None);
        connection.on_open(&Chat::default());
        assert_eq!(connection.on_visible(), None);
        connection.on_close(1006);
        assert_eq!(connection.on_visible(), Some(Action::RetryNow));
        assert_eq!(connection.state(), State::Connecting);
        // The pending timer finding the connection already retried does nothing.
        assert!(!connection.on_retry());
        assert_eq!(connection.state(), State::Connecting);
    }

    #[test]
    fn opening_without_a_hello_keeps_the_backoff() {
        let mut connection = Connection::default();
        // A backend that accepts the socket then closes it, or a proxy with the backend down.
        for expected in [1_000, 2_000, 4_000] {
            connection.on_open(&Chat::default());
            assert_eq!(connection.on_close(1006), Action::Retry(expected));
            assert!(connection.on_retry());
        }
        connection.on_open(&Chat::default());
        connection.on_hello();
        assert_eq!(connection.on_close(1006), Action::Retry(1_000));
    }

    #[test]
    fn visible_restarts_the_backoff() {
        let mut connection = Connection::default();
        for _ in 0..8 {
            connection.on_close(1006);
            connection.on_retry();
        }
        connection.on_close(1006);
        assert_eq!(connection.on_visible(), Some(Action::RetryNow));
        assert_eq!(connection.on_close(1006), Action::Retry(1_000));
    }

    #[test]
    fn retry_timer_does_nothing_once_signed_out() {
        let mut connection = Connection::default();
        connection.on_close(1006);
        connection.on_close(4401);
        assert!(!connection.on_retry());
        assert_eq!(connection.state(), State::SignedOut);
    }
}
