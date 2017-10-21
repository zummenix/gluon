use std::any::Any;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::collections::VecDeque;

use base::types::{ArcType, Type};

use {Error, Result as VmResult};
use api::{primitive, AsyncPushable, Function, Generic, Pushable, RuntimeResult, VmType, WithVM};
use api::generic::A;
use gc::{Gc, GcPtr, Traverseable};
use vm::{RootedThread, Status, Thread};
use thread::ThreadInternal;
use value::{GcStr, Userdata, Value};
use stack::{StackFrame, State};

pub struct Sender<T> {
    // No need to traverse this thread reference as any thread having a reference to this `Sender`
    // would also directly own a reference to the `Thread`
    thread: GcPtr<Thread>,
    queue: Arc<Mutex<VecDeque<T>>>,
}

impl<T> Userdata for Sender<T>
where
    T: Any + Send + Sync + fmt::Debug + Traverseable,
{
}

impl<T> fmt::Debug for Sender<T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", *self.queue.lock().unwrap())
    }
}

impl<T> Traverseable for Sender<T> {
    fn traverse(&self, _gc: &mut Gc) {
        // No need to traverse in Sender as values can only be accessed through Receiver
    }
}

impl<T> Sender<T> {
    fn send(&self, value: T) {
        self.queue.lock().unwrap().push_back(value);
    }
}

impl<T: Traverseable> Traverseable for Receiver<T> {
    fn traverse(&self, gc: &mut Gc) {
        self.queue.lock().unwrap().traverse(gc);
    }
}


pub struct Receiver<T> {
    queue: Arc<Mutex<VecDeque<T>>>,
}

impl<T> Userdata for Receiver<T>
where
    T: Any + Send + Sync + fmt::Debug + Traverseable,
{
}

impl<T> fmt::Debug for Receiver<T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", *self.queue.lock().unwrap())
    }
}

impl<T> Receiver<T> {
    fn try_recv(&self) -> Result<T, ()> {
        self.queue.lock().unwrap().pop_front().ok_or(())
    }
}

impl<T: VmType> VmType for Sender<T>
where
    T::Type: Sized,
{
    type Type = Sender<T::Type>;
    fn make_type(vm: &Thread) -> ArcType {
        let symbol = vm.global_env()
            .get_env()
            .find_type_info("Sender")
            .unwrap()
            .name
            .clone();
        Type::app(Type::ident(symbol), collect![T::make_type(vm)])
    }
}

impl<T: VmType> VmType for Receiver<T>
where
    T::Type: Sized,
{
    type Type = Receiver<T::Type>;
    fn make_type(vm: &Thread) -> ArcType {
        let symbol = vm.global_env()
            .get_env()
            .find_type_info("Receiver")
            .unwrap()
            .name
            .clone();
        Type::app(Type::ident(symbol), collect![T::make_type(vm)])
    }
}

field_decl!{ sender, receiver }


pub type ChannelRecord<S, R> = record_type!(sender => S, receiver => R);

/// FIXME The dummy `a` argument should not be needed to ensure that the channel can only be used
/// with a single type
fn channel(
    WithVM { vm, .. }: WithVM<Generic<A>>,
) -> ChannelRecord<Sender<Generic<A>>, Receiver<Generic<A>>> {
    let sender = Sender {
        thread: unsafe { GcPtr::from_raw(vm) },
        queue: Arc::new(Mutex::new(VecDeque::new())),
    };
    let receiver = Receiver {
        queue: sender.queue.clone(),
    };
    record_no_decl!(sender => sender, receiver => receiver)
}

fn recv(receiver: &Receiver<Generic<A>>) -> Result<Generic<A>, ()> {
    receiver.try_recv().map_err(|_| ())
}

fn send(sender: &Sender<Generic<A>>, value: Generic<A>) -> Result<(), ()> {
    let value = sender
        .thread
        .deep_clone_value(&sender.thread, value.0)
        .map_err(|_| ())?;
    Ok(sender.send(Generic::from(value)))
}

extern "C" fn resume(vm: &Thread) -> Status {
    let mut context = vm.context();
    let value = StackFrame::current(&mut context.stack)[0];
    match value {
        Value::Thread(child) => {
            let lock = StackFrame::current(&mut context.stack).into_lock();
            drop(context);
            let result = child.resume();
            context = vm.context();
            context.stack.release_lock(lock);
            match result {
                Ok(child_context) => {
                    // Prevent dead lock if the following status_push call allocates
                    drop(child_context);

                    let value: Result<(), &str> = Ok(());
                    value.status_push(vm, &mut context)
                }
                Err(Error::Dead) => {
                    let value: Result<(), &str> = Err("Attempted to resume a dead thread");
                    value.status_push(vm, &mut context)
                }
                Err(err) => {
                    let fmt = format!("{}", err);
                    let result = unsafe {
                        Value::String(GcStr::from_utf8_unchecked(
                            context.alloc_ignore_limit(fmt.as_bytes()),
                        ))
                    };
                    context.stack.push(result);
                    Status::Error
                }
            }
        }
        _ => unreachable!(),
    }
}

extern "C" fn yield_(_vm: &Thread) -> Status {
    Status::Yield
}

fn spawn<'vm>(
    value: WithVM<'vm, Function<&'vm Thread, fn(())>>,
) -> RuntimeResult<RootedThread, Error> {
    match spawn_(value) {
        Ok(x) => RuntimeResult::Return(x),
        Err(err) => RuntimeResult::Panic(err),
    }
}
fn spawn_<'vm>(value: WithVM<'vm, Function<&'vm Thread, fn(())>>) -> VmResult<RootedThread> {
    let thread = value.vm.new_thread()?;
    {
        let mut context = thread.context();
        let callable = match value.value.value() {
            Value::Closure(c) => State::Closure(c),
            Value::Function(c) => State::Extern(c),
            _ => State::Unknown,
        };
        value.value.push(value.vm, &mut context)?;
        context.stack.push(Value::Int(0));
        StackFrame::current(&mut context.stack).enter_scope(1, callable);
    }
    Ok(thread)
}

pub fn load<'vm>(vm: &'vm Thread) -> VmResult<()> {
    let _ = vm.register_type::<Sender<A>>("Sender", &["a"]);
    let _ = vm.register_type::<Receiver<A>>("Receiver", &["a"]);
    vm.define_global("channel", primitive!(1 channel))?;
    vm.define_global("recv", primitive!(1 recv))?;
    vm.define_global("send", primitive!(2 send))?;
    vm.define_global(
        "resume",
        primitive::<fn(&'vm Thread) -> Result<(), String>>("resume", resume),
    )?;
    vm.define_global("yield", primitive::<fn(())>("yield", yield_))?;
    vm.define_global("spawn", primitive!(1 spawn))?;
    Ok(())
}
