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

#[inline]
#[cold]
fn cold() {}

lazy_static::lazy_static! {
    static ref TABLE: RwLock<RawTable<Weak<String>>> = RwLock::new(RawTable::new());
}

type TableHasher = ahash::AHasher;

struct DisplayHasher<H: Hasher>(H);
impl<H: Hasher> DisplayHasher<H> {
    fn finish(&self) -> u64 {
        self.0.finish()
    }
}
impl<H: Hasher + Default> DisplayHasher<H> {
    fn hash<T: Display>(t: &T) -> u64 {
        use std::fmt::Write;
        let mut h = Self(H::default());
        let _ = write!(h, "{t}");
        h.finish()
    }
}
impl<H: Hasher> std::fmt::Write for DisplayHasher<H> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0.write(s.as_bytes());
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

pub struct InternedString(Arc<String>);
impl InternedString {
    pub fn intern<T: Display + Into<String>>(t: T) -> Self {
        let hash = DisplayHasher::<TableHasher>::hash(&t);
        let eq = |s: &Weak<String>| {
            if let Some(s) = Weak::upgrade(s) {
                DisplayEq::eq(&t, s.as_str())
            } else {
                false
            }
        };
        loop {
            // READ section
            {
                if let Some(s) = TABLE.read().unwrap().get(hash, eq).and_then(Weak::upgrade) {
                    return Self(s);
                }
            }
            // WRITE section
            {
                let mut guard = TABLE.write().unwrap();
                // RACE CONDITION: check again if it exists
                if let Some(s) = guard.get(hash, eq).and_then(Weak::upgrade) {
                    cold();
                    return Self(s);
                }
                // we need to create it
                let res: Arc<String> = Arc::new(t.into());
                guard.insert(hash, Arc::downgrade(&res), |s| {
                    let mut hasher = TableHasher::default();
                    if let Some(s) = Weak::upgrade(s) {
                        hasher.write(s.as_bytes())
                    }
                    hasher.finish()
                });
                return Self(res);
            }
        }
    }

    pub fn from_display<T: Display>(t: &T) -> Self {
        struct IntoString<'a, T>(&'a T);
        impl<'a, T: Display> From<IntoString<'a, T>> for String {
            fn from(value: IntoString<'a, T>) -> Self {
                value.0.to_string()
            }
        }
        impl<'a, T: Display> std::fmt::Display for IntoString<'a, T> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }
        Self::intern(IntoString(t))
    }
}

impl Drop for InternedString {
    fn drop(&mut self) {
        let s = std::mem::take(&mut self.0);
        if Arc::strong_count(&s) == 1 {
            let s = Arc::try_unwrap(s).unwrap_or_else(|s| s.deref().clone());
            let hash = DisplayHasher::<TableHasher>::hash(&s);
            let eq = |s: &Weak<String>| s.strong_count() == 0;
            let mut guard = TABLE.write().unwrap();
            if !guard.erase_entry(hash, eq) {
                cold();
                let hash = TableHasher::default().finish();
                guard.erase_entry(hash, eq);
            }
        }
    }
}

impl Deref for InternedString {
    type Target = str;
    fn deref(&self) -> &Self::Target {
        self.0.deref().deref()
    }
}

impl AsRef<[u8]> for InternedString {
    fn as_ref(&self) -> &[u8] {
        self.0.deref().as_ref()
    }
}

impl AsRef<OsStr> for InternedString {
    fn as_ref(&self) -> &OsStr {
        self.0.deref().as_ref()
    }
}

impl AsRef<Path> for InternedString {
    fn as_ref(&self) -> &Path {
        self.0.deref().as_ref()
    }
}

impl AsRef<str> for InternedString {
    fn as_ref(&self) -> &str {
        self.0.deref().as_ref()
    }
}

impl Borrow<str> for InternedString {
    fn borrow(&self) -> &str {
        self.0.deref().borrow()
    }
}

impl Clone for InternedString {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl Debug for InternedString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Debug::fmt(self.0.deref(), f)
    }
}

impl Default for InternedString {
    fn default() -> Self {
        Self::intern(String::default())
    }
}

impl Display for InternedString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Display::fmt(self.0.deref(), f)
    }
}

impl PartialEq for InternedString {
    fn eq(&self, other: &Self) -> bool {
        if Arc::ptr_eq(&self.0, &other.0) {
            true
        } else {
            self.0.deref() == other.0.deref()
        }
    }
}

impl Eq for InternedString {}

impl PartialOrd for InternedString {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        if Arc::ptr_eq(&self.0, &other.0) {
            Some(std::cmp::Ordering::Equal)
        } else {
            self.0.deref().partial_cmp(other.0.deref())
        }
    }
}

impl Ord for InternedString {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        if Arc::ptr_eq(&self.0, &other.0) {
            std::cmp::Ordering::Equal
        } else {
            self.0.deref().cmp(other.0.deref())
        }
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
        self.0.deref().hash(state)
    }
}

impl<'a> PartialEq<&'a str> for InternedString {
    fn eq(&self, other: &&'a str) -> bool {
        self.0.deref().eq(other)
    }
}

impl<'a> PartialEq<Cow<'a, str>> for InternedString {
    fn eq(&self, other: &Cow<'a, str>) -> bool {
        self.0.deref().eq(other)
    }
}

impl PartialEq<str> for InternedString {
    fn eq(&self, other: &str) -> bool {
        self.0.deref().eq(other)
    }
}
