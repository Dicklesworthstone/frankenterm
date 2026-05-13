namespace RuntimeProofSoundness

inductive Crate where
  | frankentermCore
  | downstream
  deriving DecidableEq, Repr

inductive Visibility where
  | publicVis
  | privateVis
  deriving DecidableEq, Repr

structure Trait where
  owner : Crate
  visibility : Visibility
  deriving Repr

def sealedTrait : Trait :=
  { owner := Crate.frankentermCore, visibility := Visibility.privateVis }

def runtimeProofTrait : Trait :=
  { owner := Crate.frankentermCore, visibility := Visibility.publicVis }

def canNameTrait (crate : Crate) (trait : Trait) : Prop :=
  trait.visibility = Visibility.publicVis \/ crate = trait.owner

inductive RustType where
  | mutex
  | rwLock
  | semaphore
  | broadcastSender
  | broadcastReceiver
  | oneshotSender
  | oneshotReceiver
  | joinHandle
  | joinSet
  | runtime
  | cx
  | tokioMutex
  | downstreamType
  deriving DecidableEq, Repr

def rustRuntimeProofImplNames : List String := [
  "runtime_async::Mutex<T>",
  "runtime_async::RwLock<T>",
  "runtime_async::Semaphore",
  "runtime_async::broadcast::Sender<T>",
  "runtime_async::broadcast::Receiver<T>",
  "runtime_async::oneshot::Sender<T>",
  "runtime_async::oneshot::Receiver<T>",
  "runtime_async::task::JoinHandle<T>",
  "runtime_async::task::JoinSet<T>",
  "runtime_async::Runtime",
  "Cx"
]

def declaredRuntimeProofImpl : RustType -> Prop
  | RustType.mutex => True
  | RustType.rwLock => True
  | RustType.semaphore => True
  | RustType.broadcastSender => True
  | RustType.broadcastReceiver => True
  | RustType.oneshotSender => True
  | RustType.oneshotReceiver => True
  | RustType.joinHandle => True
  | RustType.joinSet => True
  | RustType.runtime => True
  | RustType.cx => True
  | RustType.tokioMutex => False
  | RustType.downstreamType => False

def canImplementSealed (crate : Crate) (typeName : RustType) : Prop :=
  canNameTrait crate sealedTrait /\ declaredRuntimeProofImpl typeName

def canImplementRuntimeProof (crate : Crate) (typeName : RustType) : Prop :=
  canNameTrait crate runtimeProofTrait /\ canImplementSealed crate typeName

theorem downstream_cannot_name_private_sealed :
    Not (canNameTrait Crate.downstream sealedTrait) := by
  intro h
  cases h with
  | inl publicVisibility =>
      cases publicVisibility
  | inr sameOwner =>
      cases sameOwner

theorem downstream_cannot_implement_sealed (typeName : RustType) :
    Not (canImplementSealed Crate.downstream typeName) := by
  intro h
  exact downstream_cannot_name_private_sealed h.left

theorem downstream_cannot_implement_runtime_proof (typeName : RustType) :
    Not (canImplementRuntimeProof Crate.downstream typeName) := by
  intro h
  exact downstream_cannot_implement_sealed typeName h.right

theorem runtime_proof_impl_requires_declared_type
    {crate : Crate} {typeName : RustType}
    (h : canImplementRuntimeProof crate typeName) :
    declaredRuntimeProofImpl typeName :=
  h.right.right

theorem undeclared_type_cannot_implement_runtime_proof
    {crate : Crate} {typeName : RustType}
    (h : Not (declaredRuntimeProofImpl typeName)) :
    Not (canImplementRuntimeProof crate typeName) := by
  intro impl
  exact h (runtime_proof_impl_requires_declared_type impl)

theorem core_can_implement_declared_runtime_proof
    {typeName : RustType}
    (h : declaredRuntimeProofImpl typeName) :
    canImplementRuntimeProof Crate.frankentermCore typeName := by
  constructor
  · left
    rfl
  · constructor
    · right
      rfl
    · exact h

theorem tokio_mutex_is_not_declared :
    Not (declaredRuntimeProofImpl RustType.tokioMutex) := by
  intro h
  exact h

theorem tokio_mutex_cannot_implement_runtime_proof
    {crate : Crate} :
    Not (canImplementRuntimeProof crate RustType.tokioMutex) :=
  undeclared_type_cannot_implement_runtime_proof tokio_mutex_is_not_declared

theorem downstream_type_is_not_declared :
    Not (declaredRuntimeProofImpl RustType.downstreamType) := by
  intro h
  exact h

theorem downstream_type_cannot_implement_runtime_proof
    {crate : Crate} :
    Not (canImplementRuntimeProof crate RustType.downstreamType) :=
  undeclared_type_cannot_implement_runtime_proof downstream_type_is_not_declared

end RuntimeProofSoundness
