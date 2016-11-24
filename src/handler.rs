use std::time::Instant;

use Slab;
use mio::{self, Token, Ready};
use mio::timer::Timeout;
use mio::deprecated::{EventLoop, Sender};
use void::{Void, unreachable};

use scope::scope;
use {SpawnError, Scope, Response, Machine, Time, GenericScope};
use SpawnError::{NoSlabSpace};
use loop_time::{make_time, mio_timeout_ms};
use response::{decompose};


#[doc(hidden)]
pub enum Timeo {
    Fsm(Token),
}

#[doc(hidden)]
pub enum Notify {
    Fsm(Token),
}


/// Standard mio loop handler
///
///
/// # Examples
///
/// ```ignore
/// extern crate mio;
/// extern crate rotor;
///
/// let mut event_loop = mio::EventLoop::new().unwrap();
/// let mut handler = rotor::Handler::new(Context, &mut event_loop);
/// let conn = handler.add_machine_with(&mut event_loop, |scope| {
///     Ok(StateMachineConstuctor(..))
/// });
/// assert!(conn.is_ok());
/// event_loop.run(&mut handler).unwrap();
/// ```
pub struct Handler<M: Machine>
{
    slab: Slab<(Option<(Timeout, Time)>, M)>,
    context: M::Context,
    channel: Sender<Notify>,
    start_time: Instant,
}

pub fn create_handler<M: Machine>(slab: Slab<(Option<(Timeout, Time)>, M)>,
    context: M::Context, channel: Sender<Notify>)
    -> Handler<M>
{
    Handler {
        slab: slab,
        context: context,
        channel: channel,
        start_time: Instant::now(),
    }
}
pub fn set_timeout_opt<S: GenericScope>(option: Option<Time>, scope: &mut S)
    -> Option<(Timeout, Time)>
{
    option.map(|new_ts| {
        let ms = mio_timeout_ms(scope.now(), new_ts);
        let tok = scope.timeout_ms(ms)
            .expect("Can't insert a timeout. You need to \
                     increase the timer capacity");
        (tok, new_ts)
    })
}

fn replacer<C, M, N>(token: Token,
    resp: Response<M, N>, old_timeo: Option<(Timeout, Time)>,
    scope: &mut Scope<C>, creator: &mut Option<N>)
    -> Option<(Option<(Timeout, Time)>, M)>
{
    let (mach, new, newtime) = decompose(token, resp);
    let rtime = if newtime != old_timeo.clone().map(|(_, x)| x) {
        if let Some((tok, _)) = old_timeo {
            scope.clear_timeout(tok);
        }
        set_timeout_opt(newtime, scope)
    } else {
        old_timeo
    };
    *creator = new;
    mach.map(|m| (rtime, m)).ok() // the error is already logged in decompose()
}

fn replace<M, F>(slab: &mut Slab<(Option<(Timeout, Time)>, M)>, token: Token, fun: F, scope: &mut Scope<M::Context>, creator: &mut Option<M::Seed>)
    where M: Machine,
          F: FnOnce(M, &mut Scope<M::Context>) -> Response<M, M::Seed>
{
    slab.entry(token).and_then(|entry| {
      let (timeo, m) = entry.remove();
      replacer(token, fun(m, scope), timeo, scope, creator)
    }).map(|new_val|{
      let entry = slab.vacant_entry().expect("The entry was just freed.");
      entry.insert(new_val)
    });
}

fn machine_loop<M, F>(handler: &mut Handler<M>,
    eloop: &mut EventLoop<Handler<M>>, token: Token, fun: F)
    where M: Machine,
          F: FnOnce(M, &mut Scope<M::Context>) -> Response<M, M::Seed>
{
    let time = handler.loop_time();
    let ref mut context = handler.context;
    let ref mut channel = handler.channel;
    let mut creator = None;
    {
        let ref mut scope = scope(time, token, context, channel, eloop);
        replace(&mut handler.slab, token, fun, scope, &mut creator)
        // Spurious events are ok in mio
    }
    while let Some(new) = creator.take() {
        let mut new = Some(new);
        let ins = handler.slab.vacant_entry().map(|entry| {
            let token = entry.index();
            entry.insert({
              let ref mut scope = scope(time, token, context, channel, eloop);
              let (mach, newm, newtime) = decompose(token,
                  M::create(new.take().unwrap(), scope));
              newm.map(|x| unreachable(x));
              let m = mach.expect("You can't return Response::done() \
                    from Machine::create() until new release of slab crate. \
                    (requires insert_with_opt)");
              let timepair = newtime.map(|new_ts| {
                  let ms = mio_timeout_ms(scope.now(), new_ts);
                  let tok = scope.timeout_ms(ms)
                      .expect("Can't insert a timeout. You need to \
                               increase the timer capacity");
                  (tok, new_ts)
              });
              (timepair, m)
            })
        }).is_none();
        if ins {
            // TODO(tailhook) process other errors here, when they can
            // be returned from handler
            let err = NoSlabSpace(new.expect("expecting seed is still here"));

            let ref mut scope = scope(time, token, context, channel, eloop);
            replace(&mut handler.slab, token, |m, scope| m.spawn_error(scope, err), scope, &mut creator)
        } else {
            let ref mut scope = scope(time, token, context, channel, eloop);
            replace(&mut handler.slab, token, |m, scope| m.spawned(scope), scope, &mut creator)
        }
    }
    if handler.slab.is_empty() {
        eloop.shutdown();
    }
}

impl<M: Machine> Handler<M>
{
    pub fn loop_time(&self) -> Time {
        let now = Instant::now();
        return make_time(self.start_time, now);
    }
    pub fn add_machine_with<F>(&mut self, eloop: &mut EventLoop<Self>, fun: F)
        -> Result<(), SpawnError<()>>
        where F: FnOnce(&mut Scope<M::Context>) -> Response<M, Void>
    {
        let time = self.loop_time();
        let ref mut context = self.context;
        let ref mut channel = self.channel;
        let res = self.slab.vacant_entry().map(|entry| {
          let token = entry.index();
          entry.insert({
            let ref mut scope = scope(time, token, context, channel, eloop);
            let (mach, void, timeout) =  decompose(token, fun(scope));
            void.map(|x| unreachable(x));
            let m = mach.expect("You can't return Response::done() or \
                  Reponse::error() from Machine::create() until new release \
                  of slab crate. (requires insert_with_opt)");
            let to = set_timeout_opt(timeout, scope);
            (to, m)
          });
        });
        if res.is_some() {
            Ok(())
        } else {
            // TODO(tailhook) propagate error from state machine construtor
            Err(NoSlabSpace(()))
        }
    }
}

impl<M: Machine> mio::deprecated::Handler for Handler<M>
{
    type Message = Notify;
    type Timeout = Timeo;
    fn ready<'x>(&mut self, eloop: &'x mut EventLoop<Self>,
        token: Token, events: Ready)
    {
        machine_loop(self, eloop, token, |m, scope| { m.ready(events, scope) })
    }

    fn notify(&mut self, eloop: &mut EventLoop<Self>, msg: Notify) {
        match msg {
            Notify::Fsm(token) => {
                machine_loop(self, eloop, token,
                    |m, scope| { m.wakeup(scope) })
            }
        }
    }

    fn timeout(&mut self, eloop: &mut EventLoop<Self>, timeo: Timeo) {
        match timeo {
            Timeo::Fsm(token) => {
                machine_loop(self, eloop, token,
                    |m, scope| { m.timeout(scope) })
            }
        }
    }
}
