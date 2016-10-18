use std::collections::BTreeSet;
use std::io::{Error, ErrorKind};

use futures::Future;

use async;
use async::do_while::Stop;
use network::reqresp::{Outgoing, RequestBuf};

use super::resources::CoordinatorRef;
use super::requests::*;
use model::*;

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum State {
    Executor(ExecutorId),
}

pub struct Dispatch {
    coord: CoordinatorRef,
    associated: BTreeSet<State>,
    tx: Outgoing,
}

impl Dispatch {
    pub fn new(coord: CoordinatorRef, tx: Outgoing) -> Self {
        debug!("new connection");
        Dispatch {
            coord: coord,
            associated: BTreeSet::new(),
            tx: tx,
        }
    }

    pub fn dispatch(&mut self, req: RequestBuf) -> Result<(), Stop<Error>> {
        match req.name() {
            "Submission" => {
                let (req, resp) = req.decode::<Submission>()?;
                let submission = self.coord
                    .submission(req)
                    .map_err(|e| e.expect("submission promise canceled?!"))
                    .then(|res| Ok(resp.respond(res)));

                async::spawn(submission);
            }
            "AddWorkerGroup" => {
                let (AddWorkerGroup { query, group }, resp) = req.decode::<AddWorkerGroup>()?;
                let response = self.coord
                    .add_worker_group(query, group, self.tx.clone())
                    .map_err(|e| e.expect("worker group promise canceled?!"))
                    .then(|res| Ok(resp.respond(res)));
                async::spawn(response)
            }
            "AddExecutor" => {
                let (req, resp) = req.decode::<AddExecutor>()?;
                let id = self.coord.add_executor(req, self.tx.clone());
                self.associated.insert(State::Executor(id));
                resp.respond(Ok((id)));
            }
            _ => {
                let err = Error::new(ErrorKind::InvalidData, "invalid request");
                return Err(Stop::Fail(err));
            }
        }

        Ok(())
    }
}

impl Drop for Dispatch {
    fn drop(&mut self) {
        for state in &self.associated {
            match *state {
                State::Executor(id) => self.coord.remove_executor(id),
            }
        }
    }
}