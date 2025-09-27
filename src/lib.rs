use std::borrow::{Borrow, Cow};
use std::convert::Infallible;
use std::ffi::OsStr;
use std::fmt::{Debug, Display};
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, RwLock, Weak};

use hashbrown::raw::RawTable;
use tinyvec::ArrayVec;

#[cfg(feature = "serde")]
mod serde;

#[cfg(feature = "ts-rs")]
mod ts_rs;

#[inline]
#[cold]
fn cold() {}

const STACK_STR_SIZE: usize = 20;

enum StringRef {
    Heap(Weak<TableString>),
    Static(&'static str),
}

lazy_static::lazy_static! {
    static ref TABLE: RwLock<RawTable<StringRef>> = RwLock::new(RawTable::new());
}

type TableHasher = ahash::AHasher;

struct DisplayHasher<H: Hasher>(H, Option<ArrayVec<[u8; STACK_STR_SIZE]>>);
impl<H: Hasher> DisplayHasher<H> {
    fn finish(&self) -> (u64, Option<ArrayVec<[u8; STACK_STR_SIZE]>>) {
        (self.0.finish(), self.1)
    }
}
impl<H: Hasher + Default> DisplayHasher<H> {
    fn hash_and_stack<T: Display + ?Sized>(t: &T) -> (u64, Option<ArrayVec<[u8; STACK_STR_SIZE]>>) {
        use std::fmt::Write;
        let mut h = Self(H::default(), Some(ArrayVec::new()));
        let _ = write!(h, "{t}");
        h.finish()
    }
    fn hash<T: Display + ?Sized>(t: &T) -> u64 {
        use std::fmt::Write;
        let mut h = Self(H::default(), None);
        let _ = write!(h, "{t}");
        h.finish().0
    }
}
impl<H: Hasher> std::fmt::Write for DisplayHasher<H> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0.write(s.as_bytes());
        match &mut self.1 {
            None => (),
            Some(stack) if stack.len() + s.len() <= 20 => {
                stack.extend_from_slice(s.as_bytes());
            }
            x => *x = None,
        }
        Ok(())
    }
}

struct DisplayEq<'a> {
    target: &'a [u8],
}
impl<'a> DisplayEq<'a> {
    fn eq<T: Display>(src: &T, target: &'a str) -> bool {
        use std::fmt::Write;
        let mut eq = Self {
            target: target.as_bytes(),
        };
        write!(eq, "{src}").is_ok() && eq.target.is_empty()
    }
}
impl<'a> std::fmt::Write for DisplayEq<'a> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let s = s.as_bytes();
        if s.len() > self.target.len() || s != &self.target[..s.len()] {
            return Err(std::fmt::Error);
        }
        self.target = &self.target[s.len()..];
        Ok(())
    }
}

struct TableString(String);
impl Drop for TableString {
    fn drop(&mut self) {
        let hash = DisplayHasher::<TableHasher>::hash(&self.0);
        let eq = |s: &StringRef| {
            if let StringRef::Heap(s) = s
                && s.strong_count() == 0
            {
                true
            } else {
                false
            }
        };
        let mut guard = TABLE.write().unwrap();
        if !guard.erase_entry(hash, eq) {
            cold();
            let hash = TableHasher::default().finish();
            guard.erase_entry(hash, eq);
        }
    }
}

#[derive(Clone)]
enum StringRepr {
    Heap(Arc<TableString>),
    Stack(ArrayVec<[u8; STACK_STR_SIZE]>),
    Static(&'static str),
}
impl StringRepr {
    fn as_str(&self) -> &str {
        match self {
            Self::Heap(s) => s.0.as_str(),
            Self::Stack(s) => unsafe { std::str::from_utf8_unchecked(s.as_slice()) },
            Self::Static(s) => *s,
        }
    }
}
impl PartialEq for StringRepr {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Heap(a), Self::Heap(b)) => Arc::ptr_eq(a, b),
            (Self::Static(a), Self::Static(b)) => std::ptr::eq(*a, *b),
            (a, b) => a.as_str() == b.as_str(),
        }
    }
}
impl Eq for StringRepr {}
impl PartialOrd for StringRepr {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        if self == other {
            Some(std::cmp::Ordering::Equal)
        } else {
            self.as_str().partial_cmp(other.as_str())
        }
    }
}
impl Ord for StringRepr {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        if self == other {
            std::cmp::Ordering::Equal
        } else {
            self.as_str().cmp(other.as_str())
        }
    }
}

pub struct InternedString(StringRepr);
impl InternedString {
    pub fn intern<S: Display + Into<String>>(s: S) -> Self {
        let (hash, stack) = DisplayHasher::<TableHasher>::hash_and_stack(&s);
        if let Some(stack) = stack {
            return Self(StringRepr::Stack(stack));
        }
        let eq = |ts: &StringRef| match ts {
            StringRef::Heap(ts) => {
                if let Some(ts) = Weak::upgrade(ts) {
                    DisplayEq::eq(&s, ts.0.as_str())
                } else {
                    false
                }
            }
            StringRef::Static(ts) => DisplayEq::eq(&s, *ts),
        };
        // READ section
        {
            match TABLE.read().unwrap().get(hash, eq) {
                Some(StringRef::Heap(ts)) => {
                    if let Some(ts) = Weak::upgrade(ts) {
                        return Self(StringRepr::Heap(ts));
                    }
                }
                Some(StringRef::Static(ts)) => return Self(StringRepr::Static(*ts)),
                _ => (),
            }
        }
        // WRITE section
        {
            let mut guard = TABLE.write().unwrap();
            // RACE CONDITION: check again if it exists
            if let Some(ts) = guard.get_mut(hash, eq) {
                cold(); // unlikely
                match ts {
                    StringRef::Heap(ts) => {
                        if let Some(ts) = Weak::upgrade(ts) {
                            return Self(StringRepr::Heap(ts));
                        }
                    }
                    StringRef::Static(ts) => return Self(StringRepr::Static(*ts)),
                }
            }
            // we need to create it
            let res = Arc::new(TableString(s.into()));
            guard.insert(hash, StringRef::Heap(Arc::downgrade(&res)), |ts| {
                let mut hasher = TableHasher::default();
                match ts {
                    StringRef::Heap(ts) => {
                        if let Some(ts) = Weak::upgrade(ts) {
                            hasher.write(ts.0.as_bytes())
                        }
                    }
                    StringRef::Static(ts) => hasher.write(ts.as_bytes()),
                }
                hasher.finish()
            });
            Self(StringRepr::Heap(res))
        }
    }

    pub fn from_display<S: Display + ?Sized>(s: &S) -> Self {
        struct IntoString<'a, T: ?Sized>(&'a T);
        impl<'a, T: Display + ?Sized> From<IntoString<'a, T>> for String {
            fn from(value: IntoString<'a, T>) -> Self {
                value.0.to_string()
            }
        }
        impl<'a, T: Display + ?Sized> std::fmt::Display for IntoString<'a, T> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }
        Self::intern(IntoString(s))
    }

    pub fn from_static(s: &'static str) -> Self {
        Self(StringRepr::Static(s))
    }

    pub fn intern_static(s: &'static str) -> Self {
        let (hash, stack) = DisplayHasher::<TableHasher>::hash_and_stack(&s);
        if let Some(stack) = stack {
            return Self(StringRepr::Stack(stack));
        }
        let eq = |ts: &StringRef| match ts {
            StringRef::Heap(ts) => {
                if let Some(ts) = Weak::upgrade(ts) {
                    DisplayEq::eq(&s, ts.0.as_str())
                } else {
                    false
                }
            }
            StringRef::Static(ts) => DisplayEq::eq(&s, *ts),
        };
        let mut guard = TABLE.write().unwrap();

        // check if it exists
        if let Some(ts) = guard.get_mut(hash, eq) {
            if !matches!(ts, StringRef::Static(_)) {
                *ts = StringRef::Static(s);
            }
            return Self(StringRepr::Static(s));
        }

        // we need to create it
        guard.insert(hash, StringRef::Static(s), |ts| {
            let mut hasher = TableHasher::default();
            match ts {
                StringRef::Heap(ts) => {
                    if let Some(ts) = Weak::upgrade(ts) {
                        hasher.write(ts.0.as_bytes())
                    }
                }
                StringRef::Static(ts) => hasher.write(ts.as_bytes()),
            }
            hasher.finish()
        });
        Self(StringRepr::Static(s))
    }
}

impl Deref for InternedString {
    type Target = str;
    fn deref(&self) -> &Self::Target {
        self.0.as_str()
    }
}

impl AsRef<[u8]> for InternedString {
    fn as_ref(&self) -> &[u8] {
        self.0.as_str().as_ref()
    }
}

impl AsRef<OsStr> for InternedString {
    fn as_ref(&self) -> &OsStr {
        self.0.as_str().as_ref()
    }
}

impl AsRef<Path> for InternedString {
    fn as_ref(&self) -> &Path {
        self.0.as_str().as_ref()
    }
}

impl AsRef<str> for InternedString {
    fn as_ref(&self) -> &str {
        self.0.as_str()
    }
}

impl Borrow<str> for InternedString {
    fn borrow(&self) -> &str {
        self.0.as_str()
    }
}

impl<'a> Borrow<str> for &'a InternedString {
    fn borrow(&self) -> &str {
        self.0.as_str()
    }
}

impl Clone for InternedString {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl Debug for InternedString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Debug::fmt(&self.0.as_str(), f)
    }
}

impl Default for InternedString {
    fn default() -> Self {
        Self::intern(String::default())
    }
}

impl Display for InternedString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.0.as_str(), f)
    }
}

impl PartialEq for InternedString {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for InternedString {}

impl PartialOrd for InternedString {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.0.partial_cmp(&other.0)
    }
}

impl Ord for InternedString {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.cmp(&other.0)
    }
}

impl<T: Display + Into<String>> From<T> for InternedString {
    fn from(value: T) -> Self {
        Self::intern(value)
    }
}

impl FromStr for InternedString {
    type Err = Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::intern(s))
    }
}

impl Hash for InternedString {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.as_str().hash(state)
    }
}

impl<'a> PartialEq<&'a str> for InternedString {
    fn eq(&self, other: &&'a str) -> bool {
        self.0.as_str().eq(*other)
    }
}

impl<'a> PartialEq<Cow<'a, str>> for InternedString {
    fn eq(&self, other: &Cow<'a, str>) -> bool {
        self.0.as_str().eq(other)
    }
}

impl PartialEq<str> for InternedString {
    fn eq(&self, other: &str) -> bool {
        self.0.as_str().eq(other)
    }
}
