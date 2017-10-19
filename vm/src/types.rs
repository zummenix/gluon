use base::fnv::FnvMap;
use base::kind::{ArcKind, Kind, KindEnv};
use base::symbol::{Symbol, SymbolRef};
use base::types::{Alias, ArcType, RecordSelector, Type, TypeEnv};

pub use self::Instruction::*;

pub type VmIndex = u32;
pub type VmTag = u32;
pub type VmInt = isize;

/// Enum which represent the instructions executed by the virtual machine.
///
/// The binary arithmetic instructions pop two values of the stack and then push the result.
#[derive(Copy, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde_derive", derive(Deserialize, Serialize))]
pub enum Instruction {
    /// Push an integer to the stack
    PushInt(isize),
    /// Push a byte to the stack
    PushByte(u8),
    /// Push a float to the stack
    PushFloat(f64),
    /// Push a string to the stack by loading the string at `index` in the currently executing
    /// function
    PushString(VmIndex),
    /// Push a variable to the stack by loading the upvariable at `index` from the currently
    /// executing function
    PushUpVar(VmIndex),
    /// Push the value at `index`
    Push(VmIndex),
    /// Push the value at `index`
    PushGlobal(VmIndex),
    /// Call a function by passing it `args` number of arguments. The function is at the index in
    /// the stack just before the arguments. After the call is all arguments are removed and the
    /// function is replaced by the result of the call.
    Call(VmIndex),
    /// Tailcalls a function, removing the current stack frame before calling it.
    /// See `Call`.
    TailCall(VmIndex),
    /// Constructs a data value tagged by `tag` by taking the top `args` values of the stack.
    Construct {
        /// The tag of the data
        tag: VmIndex,
        /// How many arguments that is taken from the stack to construct the data.
        args: VmIndex,
    },
    ConstructRecord {
        /// Index to the specification describing which fields this record contains
        record: VmIndex,
        /// How many arguments that is taken from the stack to construct the data.
        args: VmIndex,
    },
    /// Constructs an array containing `args` values.
    ConstructArray(VmIndex),
    /// Retrieves the field at `offset` of an object at the top of the stack. The result of the
    /// field access replaces the object on the stack.
    GetOffset(VmIndex),
    /// Retrieves the field of a polymorphic record by retrieving the string constant at `index`
    /// and using that to retrieve lookup the field. The result of the
    /// field access replaces the object on the stack.
    GetField(VmIndex),
    /// Splits a object, pushing all contained values to the stack.
    Split,
    /// Tests if the value at the top of the stack is tagged with `tag`. Pushes `True` if the tag
    /// matches, otherwise `False`
    TestTag(VmTag),
    /// Jumps to the instruction at `index` in the currently executing function.
    Jump(VmIndex),
    /// Jumps to the instruction at `index` in the currently executing function if `True` is at the
    /// top of the stack and pops that value.
    CJump(VmIndex),
    /// Pops the top `n` values from the stack.
    Pop(VmIndex),
    /// Pops the top value from the stack, then pops `n` more values, finally the first value is
    /// pushed back to the stack.
    Slide(VmIndex),

    /// Creates a closure with the function at `function_index` of the currently executing function
    /// and `upvars` upvariables popped from the top of the stack.
    MakeClosure {
        /// The index in the currently executing function which the function data is located at
        function_index: VmIndex,
        /// How many upvariables the closure contains
        upvars: VmIndex,
    },
    /// Creates a closure with the function at `function_index` of the currently executing
    /// function. The closure has room for `upvars` upvariables but these are not filled until the
    /// matching call to `ClosureClosure` is executed.
    NewClosure {
        /// The index in the currently executing function which the function data is located at
        function_index: VmIndex,
        /// How many upvariables the closure contains
        upvars: VmIndex,
    },
    /// Fills the previously allocated closure with `n` upvariables.
    CloseClosure(VmIndex),

    AddInt,
    SubtractInt,
    MultiplyInt,
    DivideInt,
    IntLT,
    IntEQ,

    AddByte,
    SubtractByte,
    MultiplyByte,
    DivideByte,
    ByteLT,
    ByteEQ,

    AddFloat,
    SubtractFloat,
    MultiplyFloat,
    DivideFloat,
    FloatLT,
    FloatEQ,
}


impl Instruction {
    /// Returns by how much the stack is adjusted when executing the instruction `self`.
    pub fn adjust(&self) -> i32 {
        match *self {
            PushInt(_) | PushByte(_) | PushFloat(_) | PushString(_) | Push(_) | PushGlobal(_) => 1,
            Call(n) => -(n as i32),
            TailCall(n) => -(n as i32),
            Construct { args, .. } | ConstructRecord { args, .. } | ConstructArray(args) => {
                1 - args as i32
            }
            GetField(_) | GetOffset(_) => 0,
            // The number of added stack slots are handled separately as the type is needed to
            // calculate the number of slots needed
            Split => -1,
            TestTag(_) => 1,
            Jump(_) => 0,
            CJump(_) => -1,
            Pop(n) => -(n as i32),
            Slide(n) => -(n as i32),
            MakeClosure { .. } => 1,
            NewClosure { .. } => 1,
            CloseClosure(_) => -1,
            PushUpVar(_) => 1,
            AddInt |
            SubtractInt |
            MultiplyInt |
            DivideInt |
            IntLT |
            IntEQ |
            AddFloat |
            AddByte |
            SubtractByte |
            MultiplyByte |
            DivideByte |
            ByteLT |
            ByteEQ |
            SubtractFloat |
            MultiplyFloat |
            DivideFloat |
            FloatLT |
            FloatEQ => -1,
        }
    }
}

#[derive(Debug)]
#[cfg_attr(feature = "serde_derive", derive(DeserializeState, SerializeState))]
#[cfg_attr(feature = "serde_derive", serde(deserialize_state = "::serialization::DeSeed"))]
#[cfg_attr(feature = "serde_derive", serde(serialize_state = "::serialization::SeSeed"))]
pub struct TypeInfos {
    #[cfg_attr(feature = "serde_derive", serde(state_with = "::serialization::borrow"))]
    pub id_to_type: FnvMap<String, Alias<Symbol, ArcType>>,
}

impl KindEnv for TypeInfos {
    fn find_kind(&self, type_name: &SymbolRef) -> Option<ArcKind> {
        let type_name = AsRef::<str>::as_ref(type_name);
        self.id_to_type.get(type_name).map(|alias| {
            alias.params().iter().rev().fold(
                Kind::typ(),
                |acc, arg| Kind::function(arg.kind.clone(), acc),
            )
        })
    }
}

impl TypeEnv for TypeInfos {
    fn find_type(&self, id: &SymbolRef) -> Option<&ArcType> {
        let id = AsRef::<str>::as_ref(id);
        self.id_to_type
            .iter()
            .filter_map(|(_, ref alias)| match **alias.unresolved_type() {
                Type::Variant(ref row) => row.row_iter().find(|field| field.name.as_ref() == id),
                _ => None,
            })
            .next()
            .map(|field| &field.typ)
    }

    fn find_type_info(&self, id: &SymbolRef) -> Option<&Alias<Symbol, ArcType>> {
        let id = AsRef::<str>::as_ref(id);
        self.id_to_type.get(id)
    }

    fn find_record(
        &self,
        _fields: &[Symbol],
        _selector: RecordSelector,
    ) -> Option<(ArcType, ArcType)> {
        None
    }
}

impl TypeInfos {
    pub fn new() -> TypeInfos {
        TypeInfos {
            id_to_type: FnvMap::default(),
        }
    }

    pub fn extend(&mut self, other: TypeInfos) {
        let TypeInfos { id_to_type } = other;
        self.id_to_type.extend(id_to_type);
    }
}
