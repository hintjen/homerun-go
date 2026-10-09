//! Palworld's stop and readiness — the effects half.
//!
//! Why a Palworld server is stopped through its REST API, and every decision
//! about it, are in the pure half, `homerun_core::engine::extensions::palworld`.
//! This file sends the requests:
//!
//! | Hook | Does |
//! |---|---|
//! | `begin` | nothing: the server needs nothing from Homerun but its settings |
//! | `stop` | `POST /v1/api/save`, then `POST /v1/api/shutdown`, as `admin` with the host's admin password |
//! | `probe_ready` | `GET /v1/api/info`, the same way; any 2xx is ready |
//!
//! It keeps nothing, so `forget` and `status` are the defaults: there is no
//! sign-in to forget.

use std::collections::BTreeMap;

use homerun_core::engine::extensions::palworld::{
    self, refusal, settings, shutdown_body, Step, INFO, SAVE, SHUTDOWN, USER,
};
use serde_json::json;

use super::{
    Begun, ExtError, GameExtension, LocalRequest, ProbeContext, Run, StartContext, StopRungContext,
};
use crate::protocol::codes;

pub struct Palworld;

impl GameExtension for Palworld {
    fn name(&self) -> &'static str {
        palworld::SPEC.name
    }

    fn begin(&self, _ctx: &mut StartContext) -> Result<Begun, ExtError> {
        Ok(Begun {
            supplied: BTreeMap::new(),
            run: Box::new(PalworldRun),
        })
    }

    /// The save first, and the shutdown only once the save is answered 2xx:
    /// a shutdown after a failed save is a stop that loses the world it was
    /// meant to keep.
    fn stop(&self, ctx: &StopRungContext) -> Result<(), ExtError> {
        let settings = settings(ctx.config());
        let save = LocalRequest::post(SAVE)
            .json(json!({}))
            .basic(USER, &settings.secret);
        let answer = ctx.local_http(&settings.port, &save)?;
        if !answer.ok() {
            return Err(refused(Step::Save, answer.status));
        }
        ctx.note("Palworld saved the world");
        let shutdown = LocalRequest::post(SHUTDOWN)
            .json(shutdown_body(&settings))
            .basic(USER, &settings.secret);
        let answer = ctx.local_http(&settings.port, &shutdown)?;
        if !answer.ok() {
            return Err(refused(Step::Shutdown, answer.status));
        }
        Ok(())
    }

    /// Up once its REST API answers: the log's marker line has been seen to
    /// arrive garbled, or not at all, on Windows. Nothing listening yet, a
    /// refused password and any other status are all "not yet".
    fn probe_ready(&self, ctx: &ProbeContext) -> Result<bool, ExtError> {
        let settings = settings(ctx.config());
        let info = LocalRequest::get(INFO).basic(USER, &settings.secret);
        Ok(ctx.local_http(&settings.port, &info)?.ok())
    }
}

fn refused(step: Step, status: u16) -> ExtError {
    ExtError::new(codes::EXTENSION_FAILED, refusal(step, status))
}

/// A run needs no state: everything the stop uses is in the config.
struct PalworldRun;

impl Run for PalworldRun {}
