//! The marshalling api
use {forget_lifetime, Error, Result, Variants};
use gc::{DataDef, Gc, Move, Traverseable};
use base::symbol::Symbol;
use stack::StackFrame;
use vm::{self, Root, RootStr, RootedValue, Status, Thread};
use value::{ArrayDef, ArrayRepr, Cloner, DataStruct, Def, ExternFunction, GcStr, Value, ValueArray};
use thread::{self, Context, RootedThread};
use thread::ThreadInternal;
use base::types::{self, ArcType, Type};
use types::{VmIndex, VmInt, VmTag};

use std::any::Any;
use std::cell::Ref;
use std::cmp::Ordering;
use std::fmt;
use std::marker::PhantomData;
use std::ops::Deref;
use std::result::Result as StdResult;

use futures::{Async, Future};

pub use value::Userdata;

#[macro_use]
pub mod mac;
#[cfg(feature = "serde")]
pub mod ser;
#[cfg(feature = "serde")]
pub mod de;
#[cfg(feature = "serde")]
pub mod typ;

macro_rules! count {
    () => { 0 };
    ($_e: ident) => { 1 };
    ($_e: ident, $($rest: ident),*) => { 1 + count!($($rest),*) }
}

#[derive(Copy, Clone, Debug)]
pub enum ValueRef<'a> {
    Byte(u8),
    Int(VmInt),
    Float(f64),
    String(&'a str),
    Data(Data<'a>),
    Array(ArrayRef<'a>),
    Userdata(&'a vm::Userdata),
    Internal,
}

// Need to manually implement PartialEq so that `ValueRef`'s with different lifetimes can be compared
impl<'a, 'b> PartialEq<ValueRef<'b>> for ValueRef<'a> {
    fn eq(&self, other: &ValueRef) -> bool {
        use self::ValueRef::*;

        match (self, other) {
            (&Byte(l), &Byte(r)) => l == r,
            (&Int(l), &Int(r)) => l == r,
            (&Float(l), &Float(r)) => l == r,
            (&String(l), &String(r)) => l == r,
            (&Data(l), &Data(r)) => l == r,
            _ => false,
        }
    }
}

impl<'a> PartialEq<Value> for ValueRef<'a> {
    fn eq(&self, other: &Value) -> bool {
        self == &ValueRef::new(other)
    }
}

impl<'a> ValueRef<'a> {
    pub fn new(value: &'a Value) -> ValueRef<'a> {
        unsafe { ValueRef::rooted_new(*value) }
    }

    pub unsafe fn rooted_new(value: Value) -> ValueRef<'a> {
        match value {
            Value::Byte(i) => ValueRef::Byte(i),
            Value::Int(i) => ValueRef::Int(i),
            Value::Float(f) => ValueRef::Float(f),
            Value::String(s) => ValueRef::String(forget_lifetime(&*s)),
            Value::Data(data) => ValueRef::Data(Data(DataInner::Data(forget_lifetime(&*data)))),
            Value::Tag(tag) => ValueRef::Data(Data(DataInner::Tag(tag))),
            Value::Array(array) => ValueRef::Array(ArrayRef(forget_lifetime(&*array))),
            Value::Userdata(data) => ValueRef::Userdata(forget_lifetime(&**data)),
            Value::Thread(_) |
            Value::Function(_) |
            Value::Closure(_) |
            Value::PartialApplication(_) => ValueRef::Internal,
        }
    }
}

#[derive(Copy, Clone, PartialEq, Debug)]
enum DataInner<'a> {
    Tag(VmTag),
    Data(&'a DataStruct),
}

#[derive(Copy, Clone, PartialEq, Debug)]
pub struct Data<'a>(DataInner<'a>);

impl<'a> Data<'a> {
    pub fn tag(&self) -> VmTag {
        match self.0 {
            DataInner::Tag(tag) => tag,
            DataInner::Data(data) => data.tag(),
        }
    }

    pub fn len(&self) -> usize {
        match self.0 {
            DataInner::Tag(_) => 0,
            DataInner::Data(data) => data.fields.len(),
        }
    }

    pub fn get(&self, index: usize) -> Option<ValueRef<'a>> {
        match self.0 {
            DataInner::Tag(_) => None,
            DataInner::Data(data) => data.fields.get(index).map(ValueRef::new),
        }
    }

    pub fn get_variants(&self, index: usize) -> Option<Variants<'a>> {
        match self.0 {
            DataInner::Tag(_) => None,
            DataInner::Data(data) => unsafe { data.fields.get(index).map(|v| Variants::new(v)) },
        }
    }

    // Retrieves the field `name` from this record
    pub fn lookup_field(&self, thread: &Thread, name: &str) -> Option<Variants<'a>> {
        match self.0 {
            DataInner::Tag(_) => None,
            DataInner::Data(data) => unsafe {
                thread.lookup_field(data, name).ok().map(|v| {
                    Variants::with_root(v, data)
                })
            },
        }
    }
}

/// Marker type representing a hole
pub struct Hole(());

impl VmType for Hole {
    type Type = Hole;

    fn make_type(vm: &Thread) -> ArcType {
        vm.global_env().type_cache().hole()
    }
}

/// Type representing gluon's IO type
#[derive(Debug, PartialEq)]
pub enum IO<T> {
    Value(T),
    Exception(String),
}

pub type GluonFunction = extern "C" fn(&Thread) -> Status;

pub struct Primitive<F> {
    name: &'static str,
    function: GluonFunction,
    _typ: PhantomData<F>,
}

pub struct RefPrimitive<'vm, F> {
    name: &'static str,
    function: extern "C" fn(&'vm Thread) -> Status,
    _typ: PhantomData<F>,
}

#[inline]
pub fn primitive<F>(
    name: &'static str,
    function: extern "C" fn(&Thread) -> Status,
) -> Primitive<F> {
    Primitive {
        name: name,
        function: function,
        _typ: PhantomData,
    }
}

#[inline]
pub unsafe fn primitive_f<'vm, F>(
    name: &'static str,
    function: extern "C" fn(&'vm Thread) -> Status,
    _: F,
) -> RefPrimitive<'vm, F>
where
    F: VmFunction<'vm>,
{
    RefPrimitive {
        name: name,
        function: function,
        _typ: PhantomData,
    }
}

#[derive(PartialEq)]
pub struct Generic<T>(pub Value, PhantomData<T>);

impl<T> fmt::Debug for Generic<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<T> From<Value> for Generic<T> {
    fn from(v: Value) -> Generic<T> {
        Generic(v, PhantomData)
    }
}

impl<T: VmType> VmType for Generic<T> {
    type Type = T::Type;

    fn make_type(vm: &Thread) -> ArcType {
        T::make_type(vm)
    }

    fn extra_args() -> VmIndex {
        T::extra_args()
    }
}
impl<'vm, T: VmType> Pushable<'vm> for Generic<T> {
    fn push(self, _: &'vm Thread, context: &mut Context) -> Result<()> {
        context.stack.push(self.0);
        Ok(())
    }
}
impl<'vm, T> Getable<'vm> for Generic<T> {
    fn from_value(_: &'vm Thread, value: Variants) -> Option<Generic<T>> {
        Some(Generic::from(value.0))
    }
}

impl<T> Traverseable for Generic<T> {
    fn traverse(&self, gc: &mut Gc) {
        self.0.traverse(gc);
    }
}

/// Module containing types which represent generic variables in gluon's type system
pub mod generic {
    use super::VmType;
    use base::types::ArcType;
    use vm::Thread;
    use thread::ThreadInternal;

    macro_rules! make_generics {
        ($($i: ident)+) => {
            $(
            #[derive(Clone, Copy, PartialEq)]
            pub enum $i { }
            impl VmType for $i {
                type Type = $i;
                fn make_type(vm: &Thread) -> ArcType {
                    let s = stringify!($i);
                    let lower  = [s.as_bytes()[0] + 32];
                    let lower_str = unsafe { ::std::str::from_utf8_unchecked(&lower) };
                    vm.global_env().get_generic(lower_str)
                }
            }
            )+
        }
    }
    make_generics!{A B C D E F G H I J K L M N O P Q R X Y Z}
}

fn gather_generics(params: &mut Vec<types::Generic<Symbol>>, typ: &ArcType) {
    match **typ {
        Type::Generic(ref generic) => {
            if params.iter().all(|param| param.id != generic.id) {
                params.push(generic.clone());
            }
        }
        Type::Forall(..) => (),
        _ => {
            types::walk_move_type_opt(typ, &mut types::ControlVisitation(|typ: &ArcType| {
                gather_generics(params, typ);
                None
            }));
        }
    }
}

/// Trait which maps a type in rust to a type in gluon
pub trait VmType {
    /// A version of `Self` which implements `Any` allowing a `TypeId` to be retrieved
    type Type: ?Sized + Any;

    fn make_forall_type(vm: &Thread) -> ArcType {
        let typ = Self::make_type(vm);
        let mut params: Vec<types::Generic<_>> = Vec::new();
        gather_generics(&mut params, &typ);

        Type::forall(params, typ)
    }

    /// Creates an gluon type which maps to `Self` in rust
    fn make_type(vm: &Thread) -> ArcType {
        vm.get_type::<Self::Type>()
    }

    /// How many extra arguments a function returning this type requires.
    /// Used for abstract types which when used in return position should act like they still need
    /// more arguments before they are called
    fn extra_args() -> VmIndex {
        0
    }
}

/// Trait which allows a possibly asynchronous rust value to be pushed to the virtual machine
pub trait AsyncPushable<'vm> {
    /// Pushes `self` to `stack`. If the call is successful a single element should have been added
    /// to the stack and `Ok(())` should be returned. If the call is unsuccessful `Status:Error`
    /// should be returned and the stack should be left intact.
    ///
    /// If the value must be computed asynchronously `Async::NotReady` must be returned so that
    /// the virtual machine knows it must do more work before the value is available.
    fn async_push(self, vm: &'vm Thread, context: &mut Context) -> Result<Async<()>>;

    fn status_push(self, vm: &'vm Thread, context: &mut Context) -> Status
    where
        Self: Sized,
    {
        match self.async_push(vm, context) {
            Ok(Async::Ready(())) => Status::Ok,
            Ok(Async::NotReady) => Status::Yield,
            Err(err) => {
                let msg = unsafe {
                    GcStr::from_utf8_unchecked(
                        context.alloc_ignore_limit(format!("{}", err).as_bytes()),
                    )
                };
                context.stack.push(Value::String(msg));
                Status::Error
            }
        }
    }
}

impl<'vm, T> AsyncPushable<'vm> for T
where
    T: Pushable<'vm>,
{
    fn async_push(self, vm: &'vm Thread, context: &mut Context) -> Result<Async<()>> {
        self.push(vm, context).map(Async::Ready)
    }
}

/// Trait which allows a rust value to be pushed to the virtual machine
pub trait Pushable<'vm>: AsyncPushable<'vm> {
    /// Pushes `self` to `stack`. If the call is successful a single element should have been added
    /// to the stack and `Ok(())` should be returned. If the call is unsuccessful `Status:Error`
    /// should be returned and the stack should be left intact
    fn push(self, vm: &'vm Thread, context: &mut Context) -> Result<()>;
}

/// Trait which allows rust values to be retrieved from the virtual machine
pub trait Getable<'vm>: Sized {
    /// unsafe version of from_value which allows references to the internal of GcPtr's to be
    /// extracted if `value` is rooted
    unsafe fn from_value_unsafe(vm: &'vm Thread, value: Variants) -> Option<Self> {
        Self::from_value(vm, value)
    }
    fn from_value(vm: &'vm Thread, value: Variants) -> Option<Self>;
}

impl<'vm, T: vm::Userdata> Pushable<'vm> for T {
    fn push(self, thread: &'vm Thread, context: &mut Context) -> Result<()> {
        let data: Box<vm::Userdata> = Box::new(self);
        let userdata = context.alloc_with(thread, Move(data))?;
        context.stack.push(Value::Userdata(userdata));
        Ok(())
    }
}

impl<'vm> Getable<'vm> for Value {
    fn from_value(_vm: &'vm Thread, value: Variants) -> Option<Self> {
        Some(value.0)
    }
}

impl<'vm, T: ?Sized + VmType> VmType for &'vm T {
    type Type = T::Type;
    fn make_type(vm: &Thread) -> ArcType {
        T::make_type(vm)
    }
}

impl<'vm, T> Getable<'vm> for &'vm T
where
    T: vm::Userdata,
{
    unsafe fn from_value_unsafe(vm: &'vm Thread, value: Variants) -> Option<Self> {
        <*const T as Getable<'vm>>::from_value(vm, value).map(|p| &*p)
    }
    // Only allow the unsafe version to be used
    fn from_value(_vm: &'vm Thread, _value: Variants) -> Option<Self> {
        None
    }
}

impl<'vm> Getable<'vm> for &'vm str {
    fn from_value(_vm: &'vm Thread, value: Variants) -> Option<Self> {
        unsafe {
            match value.as_ref() {
                ValueRef::String(ref s) => Some(forget_lifetime(s)),
                _ => None,
            }
        }
    }
}

/// Wrapper type which passes acts as the type `T` but also passes the `VM` to the called function
pub struct WithVM<'vm, T> {
    pub vm: &'vm Thread,
    pub value: T,
}

impl<'vm, T> VmType for WithVM<'vm, T>
where
    T: VmType,
{
    type Type = T::Type;

    fn make_type(vm: &Thread) -> ArcType {
        T::make_type(vm)
    }

    fn extra_args() -> VmIndex {
        T::extra_args()
    }
}

impl<'vm, T> Pushable<'vm> for WithVM<'vm, T>
where
    T: Pushable<'vm>,
{
    fn push(self, vm: &'vm Thread, context: &mut Context) -> Result<()> {
        self.value.push(vm, context)
    }
}

impl<'vm, T> Getable<'vm> for WithVM<'vm, T>
where
    T: Getable<'vm>,
{
    unsafe fn from_value_unsafe(vm: &'vm Thread, value: Variants) -> Option<WithVM<'vm, T>> {
        T::from_value_unsafe(vm, value).map(|t| WithVM { vm: vm, value: t })
    }

    fn from_value(vm: &'vm Thread, value: Variants) -> Option<WithVM<'vm, T>> {
        T::from_value(vm, value).map(|t| WithVM { vm: vm, value: t })
    }
}

impl VmType for () {
    type Type = Self;
}
impl<'vm> Pushable<'vm> for () {
    fn push(self, _: &'vm Thread, context: &mut Context) -> Result<()> {
        context.stack.push(Value::Int(0));
        Ok(())
    }
}
impl<'vm> Getable<'vm> for () {
    fn from_value(_: &'vm Thread, _: Variants) -> Option<()> {
        Some(())
    }
}

impl VmType for u8 {
    type Type = Self;
}
impl<'vm> Pushable<'vm> for u8 {
    fn push(self, _: &'vm Thread, context: &mut Context) -> Result<()> {
        context.stack.push(Value::Byte(self));
        Ok(())
    }
}
impl<'vm> Getable<'vm> for u8 {
    fn from_value(_: &'vm Thread, value: Variants) -> Option<u8> {
        match value.as_ref() {
            ValueRef::Byte(i) => Some(i),
            _ => None,
        }
    }
}

macro_rules! int_impls {
    ($($id: ident)*) => {
        $(
        impl VmType for $id {
            type Type = VmInt;
        }
        impl<'vm> Pushable<'vm> for $id {
            fn push(self, _: &'vm Thread, context: &mut Context) -> Result<()> {
                context.stack.push(Value::Int(self as VmInt));
                Ok(())
            }
        }
        impl<'vm> Getable<'vm> for $id {
            fn from_value(_: &'vm Thread, value: Variants) -> Option<Self> {
                match value.as_ref() {
                    ValueRef::Int(i) => Some(i as $id),
                    _ => None,
                }
            }
        }
        )*
    };
}

int_impls!{ i16 i32 i64 u16 u32 u64 usize isize }

impl VmType for f64 {
    type Type = Self;
}
impl<'vm> Pushable<'vm> for f64 {
    fn push(self, _: &'vm Thread, context: &mut Context) -> Result<()> {
        context.stack.push(Value::Float(self));
        Ok(())
    }
}
impl<'vm> Getable<'vm> for f64 {
    fn from_value(_: &'vm Thread, value: Variants) -> Option<f64> {
        match value.as_ref() {
            ValueRef::Float(f) => Some(f),
            _ => None,
        }
    }
}
impl VmType for bool {
    type Type = Self;
    fn make_type(vm: &Thread) -> ArcType {
        (*vm.global_env()
             .get_env()
             .find_type_info("std.types.Bool")
             .unwrap())
            .clone()
            .into_type()
    }
}
impl<'vm> Pushable<'vm> for bool {
    fn push(self, _: &'vm Thread, context: &mut Context) -> Result<()> {
        context.stack.push(Value::Tag(if self { 1 } else { 0 }));
        Ok(())
    }
}
impl<'vm> Getable<'vm> for bool {
    fn from_value(_: &'vm Thread, value: Variants) -> Option<bool> {
        match value.as_ref() {
            ValueRef::Data(data) => Some(data.tag() == 1),
            _ => None,
        }
    }
}

impl VmType for Ordering {
    type Type = Self;
    fn make_type(vm: &Thread) -> ArcType {
        vm.find_type_info("std.types.Ordering")
            .unwrap()
            .clone()
            .into_type()
    }
}
impl<'vm> Pushable<'vm> for Ordering {
    fn push(self, _vm: &'vm Thread, context: &mut Context) -> Result<()> {
        let tag = match self {
            Ordering::Less => 0,
            Ordering::Equal => 1,
            Ordering::Greater => 2,
        };
        context.stack.push(Value::Tag(tag));
        Ok(())
    }
}
impl<'vm> Getable<'vm> for Ordering {
    fn from_value(_: &'vm Thread, value: Variants) -> Option<Ordering> {
        let tag = match value.as_ref() {
            ValueRef::Data(data) => data.tag(),
            _ => return None,
        };
        match tag {
            0 => Some(Ordering::Less),
            1 => Some(Ordering::Equal),
            2 => Some(Ordering::Greater),
            _ => None,
        }
    }
}

impl VmType for str {
    type Type = <String as VmType>::Type;
}
impl VmType for String {
    type Type = String;
}
impl<'vm, 's> Pushable<'vm> for &'s String {
    fn push(self, thread: &'vm Thread, context: &mut Context) -> Result<()> {
        <&str as Pushable>::push(self, thread, context)
    }
}
impl<'vm, 's> Pushable<'vm> for &'s str {
    fn push(self, thread: &'vm Thread, context: &mut Context) -> Result<()> {
        let s = unsafe { GcStr::from_utf8_unchecked(context.alloc_with(thread, self.as_bytes())?) };
        context.stack.push(Value::String(s));
        Ok(())
    }
}
impl<'vm> Getable<'vm> for String {
    fn from_value(_: &'vm Thread, value: Variants) -> Option<String> {
        match value.as_ref() {
            ValueRef::String(i) => Some(String::from(&i[..])),
            _ => None,
        }
    }
}
impl<'vm> Pushable<'vm> for String {
    fn push(self, thread: &'vm Thread, context: &mut Context) -> Result<()> {
        <&str as Pushable>::push(&self, thread, context)
    }
}

impl VmType for char {
    type Type = Self;
}
impl<'vm> Pushable<'vm> for char {
    fn push(self, _: &'vm Thread, context: &mut Context) -> Result<()> {
        context.stack.push(Value::Int(self as VmInt));
        Ok(())
    }
}
impl<'vm> Getable<'vm> for char {
    fn from_value(_: &'vm Thread, value: Variants) -> Option<char> {
        match value.as_ref() {
            ValueRef::Int(x) => ::std::char::from_u32(x as u32),
            _ => None,
        }
    }
}

impl<'s, T: VmType> VmType for Ref<'s, T> {
    type Type = T::Type;
    fn make_type(vm: &Thread) -> ArcType {
        T::make_type(vm)
    }
}

impl<'s, 'vm, T> Pushable<'vm> for Ref<'s, T>
where
    for<'t> &'t T: Pushable<'vm>,
    T: VmType,
{
    fn push(self, vm: &'vm Thread, context: &mut Context) -> Result<()> {
        <&T as Pushable>::push(&*self, vm, context)
    }
}

impl<'s, T> VmType for &'s [T]
where
    T: VmType + ArrayRepr + 's,
    T::Type: Sized,
{
    type Type = &'static [T::Type];

    fn make_type(vm: &Thread) -> ArcType {
        vm.global_env().type_cache().array(T::make_type(vm))
    }
}
impl<'vm, 's, T> Pushable<'vm> for &'s [T]
where
    T: Traverseable + Pushable<'vm> + 's,
    &'s [T]: DataDef<Value = ValueArray>,
{
    fn push(self, thread: &'vm Thread, context: &mut Context) -> Result<()> {
        let result = context.alloc_with(thread, self)?;
        context.stack.push(Value::Array(result));
        Ok(())
    }
}
impl<'s, 'vm, T: Copy + ArrayRepr> Getable<'vm> for &'s [T] {
    unsafe fn from_value_unsafe(_: &'vm Thread, value: Variants) -> Option<Self> {
        match value.as_ref() {
            ValueRef::Array(ptr) => ptr.0.as_slice().map(|s| &*(s as *const _)),
            _ => None,
        }
    }
    // Only allow the unsafe version to be used
    fn from_value(_vm: &'vm Thread, _value: Variants) -> Option<Self> {
        None
    }
}

impl<T> VmType for Vec<T>
where
    T: VmType,
    T::Type: Sized,
{
    type Type = Vec<T::Type>;

    fn make_type(thread: &Thread) -> ArcType {
        Array::<T>::make_type(thread)
    }
}

impl<'vm, T> Pushable<'vm> for Vec<T>
where
    T: Pushable<'vm>,
{
    fn push(self, thread: &'vm Thread, context: &mut Context) -> Result<()> {
        let len = self.len() as VmIndex;
        for v in self {
            if v.push(thread, context) == Err(Error::Message("Push error".into())) {
                return Err(Error::Message("Push error".into()));
            }
        }
        let result = {
            let Context {
                ref mut gc,
                ref stack,
                ..
            } = *context;
            let values = &stack[stack.len() - len..];
            thread::alloc(
                gc,
                thread,
                stack,
                ArrayDef(values)
            )?
        };
        for _ in 0..len {
            context.stack.pop();
        }
        context.stack.push(Value::Array(result));
        Ok(())
    }
}

impl<'s, T: VmType> VmType for *const T {
    type Type = T::Type;
    fn make_type(vm: &Thread) -> ArcType {
        T::make_type(vm)
    }
}

impl<'vm, T: vm::Userdata> Getable<'vm> for *const T {
    fn from_value(_: &'vm Thread, value: Variants) -> Option<*const T> {
        match value.as_ref() {
            ValueRef::Userdata(data) => data.downcast_ref::<T>().map(|x| x as *const T),
            _ => None,
        }
    }
}

impl<T: VmType> VmType for Option<T>
where
    T::Type: Sized,
{
    type Type = Option<T::Type>;
    fn make_type(vm: &Thread) -> ArcType {
        let option_alias = vm.find_type_info("std.types.Option")
            .unwrap()
            .clone()
            .into_type();
        Type::app(option_alias, collect![T::make_type(vm)])
    }
}
impl<'vm, T: Pushable<'vm>> Pushable<'vm> for Option<T> {
    fn push(self, thread: &'vm Thread, context: &mut Context) -> Result<()> {
        match self {
            Some(value) => {
                let len = context.stack.len();
                value.push(thread, context)?;
                let arg = [context.stack.pop()];
                let value = context.new_data(thread, 1, &arg)?;
                assert!(context.stack.len() == len);
                context.stack.push(value);
            }
            None => context.stack.push(Value::Tag(0)),
        }
        Ok(())
    }
}
impl<'vm, T: Getable<'vm>> Getable<'vm> for Option<T> {
    fn from_value(vm: &'vm Thread, value: Variants) -> Option<Option<T>> {
        match value.as_ref() {
            ValueRef::Data(data) => {
                if data.tag() == 0 {
                    Some(None)
                } else {
                    T::from_value(vm, data.get_variants(0).unwrap()).map(Some)
                }
            }
            _ => None,
        }
    }
}

impl<T: VmType, E: VmType> VmType for StdResult<T, E>
where
    T::Type: Sized,
    E::Type: Sized,
{
    type Type = StdResult<T::Type, E::Type>;
    fn make_type(vm: &Thread) -> ArcType {
        let result_alias = vm.find_type_info("std.types.Result")
            .unwrap()
            .clone()
            .into_type();
        Type::app(result_alias, collect![E::make_type(vm), T::make_type(vm)])
    }
}

impl<'vm, T: Pushable<'vm>, E: Pushable<'vm>> Pushable<'vm> for StdResult<T, E> {
    fn push(self, thread: &'vm Thread, context: &mut Context) -> Result<()> {
        let tag = match self {
            Ok(ok) => {
                ok.push(thread, context)?;
                1
            }
            Err(err) => {
                err.push(thread, context)?;
                0
            }
        };
        let value = context.stack.pop();
        let data = context.alloc_with(
            thread,
            Def {
                tag: tag,
                elems: &[value],
            },
        )?;
        context.stack.push(Value::Data(data));
        Ok(())
    }
}

impl<'vm, T: Getable<'vm>, E: Getable<'vm>> Getable<'vm> for StdResult<T, E> {
    fn from_value(vm: &'vm Thread, value: Variants) -> Option<StdResult<T, E>> {
        match value.as_ref() {
            ValueRef::Data(data) => {
                match data.tag() {
                    0 => E::from_value(vm, data.get_variants(0).unwrap()).map(Err),
                    1 => T::from_value(vm, data.get_variants(0).unwrap()).map(Ok),
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

/// Wrapper around a `Future` which can be used as a return value to let the virtual machine know
/// that it must resolve the `Future` to receive the value.
pub struct FutureResult<F>(pub F);

impl<F> VmType for FutureResult<F>
where
    F: Future,
    F::Item: VmType,
{
    type Type = <F::Item as VmType>::Type;
    fn make_type(vm: &Thread) -> ArcType {
        <F::Item>::make_type(vm)
    }
    fn extra_args() -> VmIndex {
        <F::Item>::extra_args()
    }
}

impl<'vm, F> AsyncPushable<'vm> for FutureResult<F>
where
    F: Future<Error = Error> + Send + 'static,
    F::Item: Pushable<'vm>,
{
    fn async_push(self, _: &'vm Thread, context: &mut Context) -> Result<Async<()>> {
        unsafe {
            context.return_future(self.0);
        }
        Ok(Async::Ready(()))
    }
}

pub enum RuntimeResult<T, E> {
    Return(T),
    Panic(E),
}

impl<T, E> From<StdResult<T, E>> for RuntimeResult<T, E> {
    fn from(result: StdResult<T, E>) -> RuntimeResult<T, E> {
        match result {
            Ok(ok) => RuntimeResult::Return(ok),
            Err(err) => RuntimeResult::Panic(err),
        }
    }
}

impl<T: VmType, E> VmType for RuntimeResult<T, E> {
    type Type = T::Type;
    fn make_type(vm: &Thread) -> ArcType {
        T::make_type(vm)
    }
}
impl<'vm, T: Pushable<'vm>, E: fmt::Display> Pushable<'vm> for RuntimeResult<T, E> {
    fn push(self, vm: &'vm Thread, context: &mut Context) -> Result<()> {
        match self {
            RuntimeResult::Return(value) => value.push(vm, context),
            RuntimeResult::Panic(err) => Err(Error::Message(format!("{}", err))),
        }
    }
}

impl<T> VmType for IO<T>
where
    T: VmType,
    T::Type: Sized,
{
    type Type = IO<T::Type>;
    fn make_type(vm: &Thread) -> ArcType {
        let env = vm.global_env().get_env();
        let alias = env.find_type_info("IO").unwrap().into_owned();
        Type::app(alias.into_type(), collect![T::make_type(vm)])
    }
    fn extra_args() -> VmIndex {
        1
    }
}

impl<'vm, T: Getable<'vm>> Getable<'vm> for IO<T> {
    fn from_value(vm: &'vm Thread, value: Variants) -> Option<IO<T>> {
        T::from_value(vm, value).map(IO::Value)
    }
}

impl<'vm, T: Pushable<'vm>> Pushable<'vm> for IO<T> {
    fn push(self, vm: &'vm Thread, context: &mut Context) -> Result<()> {
        match self {
            IO::Value(value) => value.push(vm, context),
            IO::Exception(exc) => Err(Error::Message(exc)),
        }
    }
}

/// Type which represents an array in gluon
/// Type implementing both `Pushable` and `Getable` of values of `V`.
/// The actual value, `V` is not accessible directly but is only intended to be transferred between
/// two different threads.
pub struct OpaqueValue<T, V>(RootedValue<T>, PhantomData<V>)
where
    T: Deref<Target = Thread>;

impl<T, V> fmt::Debug for OpaqueValue<T, V>
where
    T: Deref<Target = Thread>,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

impl<T, V> Clone for OpaqueValue<T, V>
where
    T: Deref<Target = Thread> + Clone,
{
    fn clone(&self) -> Self {
        OpaqueValue(self.0.clone(), self.1.clone())
    }
}

impl<T, V> OpaqueValue<T, V>
where
    T: Deref<Target = Thread>,
{
    pub fn vm(&self) -> &Thread {
        self.0.vm()
    }

    pub fn into_inner(self) -> RootedValue<T> {
        self.0
    }

    /// Unsafe as `Value` are not rooted
    pub unsafe fn get_value(&self) -> Value {
        *self.0
    }

    pub fn get_variants(&self) -> Variants {
        unsafe { Variants::new(&self.0) }
    }

    pub fn get_ref(&self) -> ValueRef {
        ValueRef::new(&self.0)
    }
}

impl<T, V> VmType for OpaqueValue<T, V>
where
    T: Deref<Target = Thread>,
    V: VmType,
    V::Type: Sized,
{
    type Type = V::Type;
    fn make_type(vm: &Thread) -> ArcType {
        V::make_type(vm)
    }
}

impl<'vm, T, V> Pushable<'vm> for OpaqueValue<T, V>
where
    T: Deref<Target = Thread>,
    V: VmType,
    V::Type: Sized,
{
    fn push(self, thread: &'vm Thread, context: &mut Context) -> Result<()> {
        let full_clone = !thread.can_share_values_with(&mut context.gc, self.0.vm());
        let mut cloner = Cloner::new(thread, &mut context.gc);
        if full_clone {
            cloner.force_full_clone();
        }
        context.stack.push(cloner.deep_clone(*self.0)?);
        Ok(())
    }
}

impl<'vm, V> Getable<'vm> for OpaqueValue<&'vm Thread, V> {
    fn from_value(vm: &'vm Thread, value: Variants) -> Option<OpaqueValue<&'vm Thread, V>> {
        Some(OpaqueValue(vm.root_value(value.0), PhantomData))
    }
}

impl<'vm, V> Getable<'vm> for OpaqueValue<RootedThread, V> {
    fn from_value(vm: &'vm Thread, value: Variants) -> Option<OpaqueValue<RootedThread, V>> {
        Some(OpaqueValue(vm.root_value(value.0), PhantomData))
    }
}

#[derive(Copy, Clone, Debug)]
pub struct ArrayRef<'vm>(&'vm ValueArray);

impl<'vm> ArrayRef<'vm> {
    pub fn get(&self, index: usize) -> Option<Variants> {
        if index < self.0.len() {
            unsafe { Some(Variants::with_root(self.0.get(index), self)) }
        } else {
            None
        }
    }

    pub fn as_slice<T>(&self) -> Option<&[T]>
    where
        T: ArrayRepr + Copy,
    {
        self.0.as_slice()
    }

    pub fn iter(&self) -> ::value::VariantIter<'vm> {
        self.0.variant_iter()
    }
}

/// Type which represents an array
pub struct Array<'vm, T>(RootedValue<&'vm Thread>, PhantomData<T>);

impl<'vm, T> Array<'vm, T> {
    pub fn vm(&self) -> &'vm Thread {
        self.0.vm_()
    }

    pub fn len(&self) -> usize {
        self.get_value_array().len()
    }

    #[doc(hidden)]
    pub fn get_value_array(&self) -> &ValueArray {
        match *self.0 {
            Value::Array(ref array) => array,
            _ => panic!("Expected an array found {:?}", self.0),
        }
    }
}

impl<'vm, T: for<'vm2> Getable<'vm2>> Array<'vm, T> {
    pub fn get(&self, index: VmInt) -> Option<T> {
        unsafe {
            match Variants::new(&self.0).as_ref() {
                ValueRef::Array(data) => {
                    let v = data.get(index as usize).unwrap();
                    T::from_value(self.0.vm(), v)
                }
                _ => None,
            }
        }
    }
}

impl<'vm, T: VmType> VmType for Array<'vm, T>
where
    T::Type: Sized,
{
    type Type = Array<'static, T::Type>;
    fn make_type(vm: &Thread) -> ArcType {
        vm.global_env().type_cache().array(T::make_type(vm))
    }
}


impl<'vm, T: VmType> Pushable<'vm> for Array<'vm, T>
where
    T::Type: Sized,
{
    fn push(self, _: &'vm Thread, context: &mut Context) -> Result<()> {
        context.stack.push(*self.0);
        Ok(())
    }
}

impl<'vm, T> Getable<'vm> for Array<'vm, T> {
    fn from_value(vm: &'vm Thread, value: Variants) -> Option<Array<'vm, T>> {
        Some(Array(vm.root_value(value.0), PhantomData))
    }
}

impl<'vm, T: Any> VmType for Root<'vm, T> {
    type Type = T;
}
impl<'vm, T: vm::Userdata> Getable<'vm> for Root<'vm, T> {
    fn from_value(vm: &'vm Thread, value: Variants) -> Option<Root<'vm, T>> {
        match value.0 {
            Value::Userdata(data) => vm.root::<T>(data).map(From::from),
            _ => None,
        }
    }
}

impl<'vm> VmType for RootStr<'vm> {
    type Type = <str as VmType>::Type;
}
impl<'vm> Getable<'vm> for RootStr<'vm> {
    fn from_value(vm: &'vm Thread, value: Variants) -> Option<RootStr<'vm>> {
        match value.0 {
            Value::String(v) => Some(vm.root_string(v)),
            _ => None,
        }
    }
}

/// NewType which can be used to push types implementating `AsRef`
/// Newtype which can be used to push types implementating `AsRef`
pub struct PushAsRef<T, R: ?Sized>(T, PhantomData<R>);

impl<T, R: ?Sized> PushAsRef<T, R> {
    pub fn new(value: T) -> PushAsRef<T, R> {
        PushAsRef(value, PhantomData)
    }
}

impl<T, R: ?Sized> VmType for PushAsRef<T, R>
where
    T: AsRef<R>,
    R: 'static,
    &'static R: VmType,
{
    type Type = <&'static R as VmType>::Type;

    fn make_type(thread: &Thread) -> ArcType {
        <&'static R as VmType>::make_type(thread)
    }
}

impl<'vm, T, R: ?Sized> Pushable<'vm> for PushAsRef<T, R>
where
    T: AsRef<R>,
    for<'a> &'a R: Pushable<'vm>,
{
    fn push(self, thread: &'vm Thread, context: &mut Context) -> Result<()> {
        self.0.as_ref().push(thread, context)
    }
}

macro_rules! define_tuple {
    ($($id: ident)+) => {
        impl<$($id),+> VmType for ($($id),+)
        where $($id: VmType),+,
              $($id::Type: Sized),+
        {
            type Type = ($($id::Type),+);

            fn make_type(vm: &Thread) -> ArcType {
                let type_cache = vm.global_env().type_cache();
                type_cache.record(Vec::new(),
                             vec![$(types::Field::new(Symbol::from(stringify!($id)),
                                                      $id::make_type(vm))),+])
            }
        }
        impl<'vm, $($id: Getable<'vm>),+> Getable<'vm> for ($($id),+) {
            #[allow(unused_assignments)]
            fn from_value(vm: &'vm Thread, value: Variants) -> Option<($($id),+)> {
                match value.as_ref() {
                    ValueRef::Data(v) => {
                        assert!(v.len() == count!($($id),+));
                        let mut i = 0;
                        let x = ( $(
                            { let a = $id::from_value(vm, v.get_variants(i).unwrap()); i += 1; a }
                        ),+ );
                        match x {
                            ($(Some($id)),+) => Some(( $($id),+ )),
                            _ => None,
                        }
                    }
                    _ => None,
                }
            }
        }
        impl<'vm, $($id),+> Pushable<'vm> for ($($id),+)
        where $($id: Pushable<'vm>),+
        {
            fn push(self, thread: &'vm Thread, context: &mut Context) -> Result<()> {
                let ( $($id),+ ) = self;
                $(
                    $id.push(thread, context)?;
                )+
                let len = count!($($id),+);
                let offset = context.stack.len() - len;
                let value = thread::alloc(&mut context.gc,
                                          thread,
                                          &context.stack,
                                          Def {
                                              tag: 0,
                                              elems: &context.stack[offset..],
                                          })?;
                for _ in 0..len {
                    context.stack.pop();
                }
                context.stack.push(Value::Data(value)) ;
                Ok(())
            }
        }
    }
}

macro_rules! define_tuples {
    ($first: ident) => { };
    ($first: ident $($rest: ident)+) => {
        define_tuple!{ $first $($rest)+ }
        define_tuples!{ $($rest)+ }
    }
}
define_tuples! { _0 _1 _2 _3 _4 _5 _6 }


pub use self::record::Record;

pub mod record {
    use std::any::Any;

    use frunk_core::hlist::{h_cons, HCons, HList, HNil, Plucker};

    use base::types;
    use base::types::ArcType;
    use base::symbol::Symbol;

    use {Result, Variants};
    use thread::{self, Context, ThreadInternal};
    use types::VmIndex;
    use vm::Thread;
    use value::{Def, Value};
    use super::{Getable, Pushable, VmType};

    pub struct Record<T> {
        pub fields: T,
    }

    pub trait Field: Default {
        fn name() -> &'static str;
    }

    pub trait FieldTypes: HList {
        type Type: Any;
        fn field_types(vm: &Thread, fields: &mut Vec<types::Field<Symbol, ArcType>>);
    }

    pub trait PushableFieldList<'vm>: HList {
        fn push(self, vm: &'vm Thread, fields: &mut Context) -> Result<()>;
    }

    pub trait GetableFieldList<'vm>: HList + Sized {
        fn from_value(vm: &'vm Thread, values: &[Value]) -> Option<Self>;
    }

    impl<'vm> PushableFieldList<'vm> for HNil {
        fn push(self, _: &'vm Thread, _: &mut Context) -> Result<()> {
            Ok(())
        }
    }

    impl<'vm> GetableFieldList<'vm> for HNil {
        fn from_value(_vm: &'vm Thread, values: &[Value]) -> Option<Self> {
            debug_assert!(values.is_empty(), "{:?}", values);
            Some(HNil)
        }
    }

    impl FieldTypes for HNil {
        type Type = ();
        fn field_types(_: &Thread, _: &mut Vec<types::Field<Symbol, ArcType>>) {}
    }

    impl<F: Field, H: VmType, T> FieldTypes for HCons<(F, H), T>
    where
        T: FieldTypes,
        H::Type: Sized,
    {
        type Type = HCons<(&'static str, H::Type), T::Type>;
        fn field_types(vm: &Thread, fields: &mut Vec<types::Field<Symbol, ArcType>>) {
            fields.push(types::Field::new(Symbol::from(F::name()), H::make_forall_type(vm)));
            T::field_types(vm, fields);
        }
    }

    impl<'vm, F: Field, H: Pushable<'vm>, T> PushableFieldList<'vm> for HCons<(F, H), T>
    where
        T: PushableFieldList<'vm>,
    {
        fn push(self, vm: &'vm Thread, fields: &mut Context) -> Result<()> {
            let ((_, head), tail) = self.pluck();
            head.push(vm, fields)?;
            tail.push(vm, fields)
        }
    }

    impl<'vm, F, H, T> GetableFieldList<'vm> for HCons<(F, H), T>
    where
        F: Field,
        H: Getable<'vm> + VmType,
        T: GetableFieldList<'vm>,
    {
        fn from_value(vm: &'vm Thread, values: &[Value]) -> Option<Self> {
            let head = unsafe { H::from_value(vm, Variants::new(&values[0])) };
            head.and_then(|head| {
                T::from_value(vm, &values[1..]).map(move |tail| h_cons((F::default(), head), tail))
            })
        }
    }

    impl<T: FieldTypes> VmType for Record<T> {
        type Type = Record<T::Type>;
        fn make_type(vm: &Thread) -> ArcType {
            let len = T::static_len();
            let mut fields = Vec::with_capacity(len);
            T::field_types(vm, &mut fields);
            let type_cache = vm.global_env().type_cache();
            type_cache.record(Vec::new(), fields)
        }
    }
    impl<'vm, T> Pushable<'vm> for Record<T>
    where
        T: PushableFieldList<'vm>,
    {
        fn push(self, thread: &'vm Thread, context: &mut Context) -> Result<()> {
            self.fields.push(thread, context)?;
            let len = T::static_len() as VmIndex;
            let offset = context.stack.len() - len;
            let value = thread::alloc(
                &mut context.gc,
                thread,
                &context.stack,
                Def {
                    tag: 0,
                    elems: &context.stack[offset..],
                },
            )?;
            for _ in 0..len {
                context.stack.pop();
            }
            context.stack.push(Value::Data(value));
            Ok(())
        }
    }
    impl<'vm, T> Getable<'vm> for Record<T>
    where
        T: GetableFieldList<'vm>,
    {
        fn from_value(vm: &'vm Thread, value: Variants) -> Option<Self> {
            match value.0 {
                Value::Data(ref data) => {
                    T::from_value(vm, &data.fields).map(|fields| Record { fields: fields })
                }
                _ => None,
            }
        }
    }
}

impl<'vm, F: VmType> VmType for Primitive<F> {
    type Type = F::Type;
    fn make_type(vm: &Thread) -> ArcType {
        F::make_type(vm)
    }
}

impl<'vm, F> Pushable<'vm> for Primitive<F>
where
    F: FunctionType + VmType,
{
    fn push(self, thread: &'vm Thread, context: &mut Context) -> Result<()> {
        // Map rust modules into gluon modules
        let id = Symbol::from(self.name.replace("::", "."));
        let value = Value::Function(context.alloc_with(
            thread,
            Move(ExternFunction {
                id: id,
                args: F::arguments(),
                function: self.function,
            }),
        )?);
        context.stack.push(value);
        Ok(())
    }
}

impl<'vm, F: VmType> VmType for RefPrimitive<'vm, F> {
    type Type = F::Type;
    fn make_type(vm: &Thread) -> ArcType {
        F::make_type(vm)
    }
}


impl<'vm, F> Pushable<'vm> for RefPrimitive<'vm, F>
where
    F: VmFunction<'vm> + FunctionType + VmType + 'vm,
{
    fn push(self, thread: &'vm Thread, context: &mut Context) -> Result<()> {
        use std::mem::transmute;
        let extern_function = unsafe {
            // The VM guarantess that it only ever calls this function with itself which should
            // make sure that ignoring the lifetime is safe
            transmute::<extern "C" fn(&'vm Thread) -> Status, extern "C" fn(&Thread) -> Status>(
                self.function,
            )
        };
        Primitive {
            function: extern_function,
            name: self.name,
            _typ: self._typ,
        }.push(thread, context)
    }
}

pub struct CPrimitive {
    function: GluonFunction,
    args: VmIndex,
    id: Symbol,
}

impl CPrimitive {
    pub unsafe fn new(function: GluonFunction, args: VmIndex, id: &str) -> CPrimitive {
        CPrimitive {
            id: Symbol::from(id),
            function: function,
            args: args,
        }
    }
}

impl<'vm> Pushable<'vm> for CPrimitive {
    fn push(self, thread: &'vm Thread, context: &mut Context) -> Result<()> {
        use std::mem::transmute;
        let function = self.function;
        let extern_function = unsafe {
            // The VM guarantess that it only ever calls this function with itself which should
            // make sure that ignoring the lifetime is safe
            transmute::<extern "C" fn(&'vm Thread) -> Status, extern "C" fn(&Thread) -> Status>(
                function,
            )
        };
        let value = context.alloc_with(
            thread,
            Move(ExternFunction {
                id: self.id,
                args: self.args,
                function: extern_function,
            }),
        )?;
        context.stack.push(Value::Function(value));
        Ok(())
    }
}


fn make_type<T: ?Sized + VmType>(vm: &Thread) -> ArcType {
    <T as VmType>::make_type(vm)
}

/// Type which represents a function reference in gluon
pub type FunctionRef<'vm, F> = Function<&'vm Thread, F>;

/// Type which represents an function in gluon
pub struct Function<T, F>
where
    T: Deref<Target = Thread>,
{
    value: RootedValue<T>,
    _marker: PhantomData<F>,
}

impl<T, F> Clone for Function<T, F>
where
    T: Deref<Target = Thread> + Clone,
{
    fn clone(&self) -> Self {
        Function {
            value: self.value.clone(),
            _marker: self._marker.clone(),
        }
    }
}

impl<T, F> Function<T, F>
where
    T: Deref<Target = Thread>,
{
    pub fn value(&self) -> Value {
        *self.value
    }
}

impl<T, F> VmType for Function<T, F>
where
    T: Deref<Target = Thread>,
    F: VmType,
{
    type Type = F::Type;
    fn make_type(vm: &Thread) -> ArcType {
        F::make_type(vm)
    }
}

impl<'vm, T, F: Any> Pushable<'vm> for Function<T, F>
where
    T: Deref<Target = Thread>,
    F: VmType,
{
    fn push(self, _: &'vm Thread, context: &mut Context) -> Result<()> {
        context.stack.push(*self.value);
        Ok(())
    }
}

impl<'vm, F> Getable<'vm> for Function<&'vm Thread, F> {
    fn from_value(vm: &'vm Thread, value: Variants) -> Option<Function<&'vm Thread, F>> {
        Some(Function {
            value: vm.root_value(value.0),
            _marker: PhantomData,
        }) //TODO not type safe
    }
}

impl<'vm, F> Getable<'vm> for Function<RootedThread, F> {
    fn from_value(vm: &'vm Thread, value: Variants) -> Option<Self> {
        Some(Function {
            value: vm.root_value(value.0),
            _marker: PhantomData,
        }) //TODO not type safe
    }
}

/// Trait which represents a function
pub trait FunctionType {
    /// Returns how many arguments the function needs to be provided to call it
    fn arguments() -> VmIndex;
}

/// Trait which abstracts over types which can be called by being pulling the arguments it needs
/// from the virtual machine's stack
pub trait VmFunction<'vm> {
    fn unpack_and_call(&self, vm: &'vm Thread) -> Status;
}

impl<'s, T: FunctionType> FunctionType for &'s T {
    fn arguments() -> VmIndex {
        T::arguments()
    }
}

impl<'vm, 's, T: ?Sized> VmFunction<'vm> for &'s T
where
    T: VmFunction<'vm>,
{
    fn unpack_and_call(&self, vm: &'vm Thread) -> Status {
        (**self).unpack_and_call(vm)
    }
}

impl<F> FunctionType for Box<F>
where
    F: FunctionType,
{
    fn arguments() -> VmIndex {
        F::arguments()
    }
}

impl<'vm, F: ?Sized> VmFunction<'vm> for Box<F>
where
    F: VmFunction<'vm>,
{
    fn unpack_and_call(&self, vm: &'vm Thread) -> Status {
        (**self).unpack_and_call(vm)
    }
}

macro_rules! make_vm_function {
    ($($args:ident),*) => (
impl <$($args: VmType,)* R: VmType> VmType for fn ($($args),*) -> R {
    #[allow(non_snake_case)]
    type Type = fn ($($args::Type),*) -> R::Type;

    #[allow(non_snake_case)]
    fn make_type(vm: &Thread) -> ArcType {
        let args = vec![$(make_type::<$args>(vm)),*];
        vm.global_env().type_cache().function(args, make_type::<R>(vm))
    }
}

impl <'vm, $($args,)* R: VmType> FunctionType for fn ($($args),*) -> R {
    fn arguments() -> VmIndex {
        count!($($args),*) + R::extra_args()
    }
}

impl <'vm, $($args,)* R> VmFunction<'vm> for fn ($($args),*) -> R
where $($args: Getable<'vm> + 'vm,)*
      R: AsyncPushable<'vm> + VmType + 'vm
{
    #[allow(non_snake_case, unused_mut, unused_assignments, unused_variables, unused_unsafe)]
    fn unpack_and_call(&self, vm: &'vm Thread) -> Status {
        let n_args = Self::arguments();
        let mut context = vm.context();
        let mut i = 0;
        let r = unsafe {
            let (lock, ($($args,)*)) = {
                let stack = StackFrame::current(&mut context.stack);
                $(let $args = {
                    let x = $args::from_value_unsafe(vm, Variants::new(&stack[i]))
                        .expect(stringify!(Argument $args));
                    i += 1;
                    x
                });*;
// Lock the frame to ensure that any reference from_value_unsafe may have returned stay
// rooted
                (stack.into_lock(), ($($args,)*))
            };
            drop(context);
            let r = (*self)($($args),*);
            context = vm.context();
            context.stack.release_lock(lock);
            r
        };
        r.status_push(vm, &mut context)
    }
}

impl <'s, $($args,)* R: VmType> FunctionType for Fn($($args),*) -> R + 's {
    fn arguments() -> VmIndex {
        count!($($args),*) + R::extra_args()
    }
}

impl <'s, $($args: VmType,)* R: VmType> VmType for Fn($($args),*) -> R + 's {
    type Type = fn ($($args::Type),*) -> R::Type;

    #[allow(non_snake_case)]
    fn make_type(vm: &Thread) -> ArcType {
        <fn ($($args),*) -> R>::make_type(vm)
    }
}

impl <'vm, $($args,)* R> VmFunction<'vm> for Fn($($args),*) -> R + 'vm
where $($args: Getable<'vm> + 'vm,)*
      R: AsyncPushable<'vm> + VmType + 'vm
{
    #[allow(non_snake_case, unused_mut, unused_assignments, unused_variables, unused_unsafe)]
    fn unpack_and_call(&self, vm: &'vm Thread) -> Status {
        let n_args = Self::arguments();
        let mut context = vm.context();
        let mut i = 0;
        let r = unsafe {
            let (lock, ($($args,)*)) = {
                let stack = StackFrame::current(&mut context.stack);
                $(let $args = {
                    let x = $args::from_value_unsafe(vm, Variants::new(&stack[i]))
                        .expect(stringify!(Argument $args));
                    i += 1;
                    x
                });*;
// Lock the frame to ensure that any reference from_value_unsafe may have returned stay
// rooted
                (stack.into_lock(), ($($args,)*))
            };
            drop(context);
            let r = (*self)($($args),*);
            context = vm.context();
            context.stack.release_lock(lock);
            r
        };
        r.status_push(vm, &mut context)
    }
}

impl<'vm, T, $($args,)* R> Function<T, fn($($args),*) -> R>
    where $($args: Pushable<'vm>,)*
          T: Deref<Target = Thread>,
          R: VmType + for<'x> Getable<'x>,
{
    #[allow(non_snake_case)]
    pub fn call(&'vm mut self $(, $args: $args)*) -> Result<R> {
        match self.call_first($($args),*)? {
            Async::Ready(value) => Ok(value),
            Async::NotReady => Err(Error::Message("Unexpected async".into())),
        }
    }

    #[allow(non_snake_case)]
    fn call_first(&'vm self $(, $args: $args)*) -> Result<Async<R>> {
        let vm = self.value.vm();
        let mut context = vm.context();
        context.stack.push(*self.value);
        $(
            $args.push(&vm, &mut context)?;
        )*
        for _ in 0..R::extra_args() {
            0.push(&vm, &mut context).unwrap();
        }
        let args = count!($($args),*) + R::extra_args();
        vm.call_function(context, args).and_then(|async|{
            match async {
                Async::Ready(context) => {
                    let value = context.unwrap().stack.pop();
                    Self::return_value(vm, value).map(Async::Ready)
                }
                Async::NotReady => Ok(Async::NotReady),
            }
        })
    }

    fn return_value(vm: &Thread, value: Value) -> Result<R> {
        unsafe {
            R::from_value(vm, Variants::new(&value))
                .ok_or_else(|| {
                    error!("Wrong type {:?}", value);
                    Error::Message("Wrong type".to_string())
                })
        }
    }
}

impl<'vm, T, $($args,)* R> Function<T, fn($($args),*) -> R>
    where $($args: Pushable<'vm>,)*
          T: Deref<Target = Thread> + Clone + Send + 'static,
          R: VmType + for<'x> Getable<'x> + Send + 'static,
{
    #[allow(non_snake_case)]
    pub fn call_async(&'vm mut self $(, $args: $args)*) -> Box<Future<Item = R, Error = Error> + Send + 'static> {
        use futures::{failed, finished};
        use futures::future::Either;
        use thread::Execute;

        Box::new(match self.call_first($($args),*) {
            Ok(ok) => {
                Either::A(match ok {
                    Async::Ready(value) => Either::A(finished(value)),
                    Async::NotReady => {
                        Either::B(Execute::new(self.value.clone_vm())
                            .and_then(|(vm, value)| Self::return_value(&vm, value)))
                    }
                })
            }
            Err(err) => {
                Either::B(failed(err))
            }
        })
    }
}
    )
}

make_vm_function!();
make_vm_function!(A);
make_vm_function!(A, B);
make_vm_function!(A, B, C);
make_vm_function!(A, B, C, D);
make_vm_function!(A, B, C, D, E);
make_vm_function!(A, B, C, D, E, F);
make_vm_function!(A, B, C, D, E, F, G);
