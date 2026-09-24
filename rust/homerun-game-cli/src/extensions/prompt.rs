//! Asking the person a question, and waiting for the answer.
//!
//! An extension's `begin` sometimes needs a choice only the person can make
//! -- which of their account's profiles hosts this server, say. It asks
//! through [`super::StartContext::prompt`]; the runner sends a generic
//! `prompt` event, the host shows its one choice dialog, and the answer
//! comes back as a `prompt-answer` command on the main thread. [`Prompts`]
//! is the meeting point between the two threads.
//!
//! One prompt is open at a time, because one server runs at a time and only
//! `begin` may ask. Every prompt ends with `prompt-closed` -- answered, or
//! abandoned by a stop -- so a host always knows to take its dialog down.

use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    time::Duration,
};

use homerun_supervisor::engine::StopSignal;
use serde::Serialize;

use super::ExtError;
use crate::{
    prepare::{fail, Failure},
    protocol::{codes, Event},
    runner::Output,
};

/// A question with a closed set of answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub title: String,
    pub message: Option<String>,
    pub options: Vec<Choice>,
}

/// One answer, and how the host shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Choice {
    pub value: String,
    pub label: String,
}

/// The open prompt, shared by the lifecycle thread (which waits on it) and
/// the main thread (which answers it).
#[derive(Clone, Default)]
pub struct Prompts(Arc<Mutex<Option<Open>>>);

struct Open {
    server_id: String,
    prompt_id: String,
    values: Vec<String>,
    answer: mpsc::Sender<String>,
}

impl Prompts {
    /// A `prompt-answer` command.
    ///
    /// An answer for a prompt that is no longer open is not an error: a
    /// person who clicks as the server is stopped has done nothing wrong,
    /// and the host has `prompt-closed` to go on. An answer that is not one
    /// of the choices is.
    pub fn answer(&self, server_id: &str, prompt_id: &str, value: String) -> Result<(), Failure> {
        let open = self.0.lock().unwrap();
        let Some(open) = open
            .as_ref()
            .filter(|o| o.server_id == server_id && o.prompt_id == prompt_id)
        else {
            eprintln!("An answer arrived for a prompt that is no longer open.");
            return Ok(());
        };
        if !open.values.contains(&value) {
            return Err(fail(
                codes::PROMPT_INVALID,
                "That is not one of the choices offered.",
            ));
        }
        let _ = open.answer.send(value);
        Ok(())
    }

    /// Ask, and block until an answer or a stop. See the module header.
    pub(super) fn ask(
        &self,
        server_id: &str,
        prompt: Prompt,
        stop: &StopSignal,
        out: &Output,
    ) -> Result<String, ExtError> {
        let values: Vec<String> = prompt.options.iter().map(|c| c.value.clone()).collect();
        let distinct: std::collections::BTreeSet<&String> = values.iter().collect();
        if values.is_empty()
            || values.iter().any(String::is_empty)
            || distinct.len() != values.len()
        {
            eprintln!("An extension asked a question without distinct, non-empty choices.");
            return Err(ExtError::new(
                codes::EXTENSION_FAILED,
                "Homerun hit a problem getting this game ready. Try starting it again.",
            ));
        }
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let prompt_id = format!("prompt-{}", NEXT.fetch_add(1, Ordering::SeqCst));
        let (answer, answers) = mpsc::channel();
        {
            let mut open = self.0.lock().unwrap();
            if open.is_some() {
                eprintln!("An extension asked a second question while one was open.");
                return Err(ExtError::new(
                    codes::EXTENSION_FAILED,
                    "Homerun hit a problem getting this game ready. Try starting it again.",
                ));
            }
            *open = Some(Open {
                server_id: server_id.into(),
                prompt_id: prompt_id.clone(),
                values,
                answer,
            });
        }
        out.send(Event::Prompt {
            server_id: server_id.into(),
            prompt_id: prompt_id.clone(),
            kind: "choice".into(),
            title: prompt.title,
            message: prompt.message,
            options: prompt.options,
        });
        // No overall limit: a person may take as long as they like, and a
        // Stop is how they say they will not answer.
        let result = loop {
            match answers.recv_timeout(Duration::from_millis(50)) {
                Ok(value) => break Ok(value),
                Err(mpsc::RecvTimeoutError::Timeout) if !stop.should_stop() => continue,
                Err(_) => break Err(ExtError::cancelled()),
            }
        };
        *self.0.lock().unwrap() = None;
        out.send(Event::PromptClosed {
            server_id: server_id.into(),
            prompt_id,
        });
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Mutex as StdMutex, thread};

    fn recording() -> (Output, Arc<StdMutex<Vec<Event>>>) {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let s = seen.clone();
        (Output::new(move |e| s.lock().unwrap().push(e)), seen)
    }

    fn two_choices() -> Prompt {
        Prompt {
            title: "Which profile hosts this server?".into(),
            message: None,
            options: vec![
                Choice {
                    value: "a".into(),
                    label: "Alpha".into(),
                },
                Choice {
                    value: "b".into(),
                    label: "Beta".into(),
                },
            ],
        }
    }

    fn open_id(seen: &Arc<StdMutex<Vec<Event>>>) -> Option<String> {
        seen.lock().unwrap().iter().find_map(|e| match e {
            Event::Prompt { prompt_id, .. } => Some(prompt_id.clone()),
            _ => None,
        })
    }

    #[test]
    fn an_answer_among_the_choices_is_returned_and_the_prompt_closed() {
        let prompts = Prompts::default();
        let (out, seen) = recording();
        let (p, s) = (prompts.clone(), seen.clone());
        let answerer = thread::spawn(move || loop {
            if let Some(id) = open_id(&s) {
                assert!(p.answer("s1", &id, "zzz".into()).is_err(), "not a choice");
                p.answer("s1", &id, "b".into()).unwrap();
                return;
            }
            thread::sleep(Duration::from_millis(10));
        });
        let got = prompts
            .ask("s1", two_choices(), &StopSignal::default(), &out)
            .unwrap();
        answerer.join().unwrap();
        assert_eq!(got, "b");
        assert!(matches!(
            seen.lock().unwrap().last(),
            Some(Event::PromptClosed { .. })
        ));
        // Closed means closed: a late answer is not an error, and goes nowhere.
        let id = open_id(&seen).unwrap();
        assert!(prompts.answer("s1", &id, "a".into()).is_ok());
    }

    #[test]
    fn a_stop_abandons_the_prompt_and_closes_it() {
        let prompts = Prompts::default();
        let (out, seen) = recording();
        let stop = StopSignal::default();
        // On its own thread, so a prompt that ignores the stop fails this
        // test instead of hanging the suite.
        let (done, finished) = mpsc::channel();
        let (p, s) = (prompts.clone(), stop.clone());
        thread::spawn(move || {
            let _ = done.send(p.ask("s1", two_choices(), &s, &out).is_err());
        });
        thread::sleep(Duration::from_millis(100));
        stop.request_stop();
        let ended = finished
            .recv_timeout(Duration::from_secs(5))
            .expect("a stop must end an open prompt");
        assert!(ended, "a stopped prompt is an error, not an answer");
        assert!(matches!(
            seen.lock().unwrap().last(),
            Some(Event::PromptClosed { .. })
        ));
    }

    #[test]
    fn choices_must_be_distinct_and_non_empty() {
        let (out, _) = recording();
        for options in [
            vec![],
            vec![("", "Nothing")],
            vec![("a", "A"), ("a", "Also A")],
        ] {
            let prompt = Prompt {
                title: "?".into(),
                message: None,
                options: options
                    .into_iter()
                    .map(|(v, l)| Choice {
                        value: v.into(),
                        label: l.into(),
                    })
                    .collect(),
            };
            assert!(Prompts::default()
                .ask("s1", prompt, &StopSignal::default(), &out)
                .is_err());
        }
    }

    #[test]
    fn an_answer_for_another_server_or_prompt_goes_nowhere() {
        let prompts = Prompts::default();
        assert!(prompts.answer("s1", "prompt-0", "a".into()).is_ok());
    }
}
