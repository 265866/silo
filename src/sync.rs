use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

pub trait MutexExt<T> {
    fn lock_recover(&self) -> MutexGuard<'_, T>;
}

impl<T> MutexExt<T> for Mutex<T> {
    fn lock_recover(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

pub trait RwLockExt<T> {
    fn read_recover(&self) -> RwLockReadGuard<'_, T>;
    fn write_recover(&self) -> RwLockWriteGuard<'_, T>;
}

impl<T> RwLockExt<T> for RwLock<T> {
    fn read_recover(&self) -> RwLockReadGuard<'_, T> {
        self.read().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write_recover(&self) -> RwLockWriteGuard<'_, T> {
        self.write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn recovers_from_poison() {
        let m = Arc::new(Mutex::new(7i32));
        let m2 = m.clone();
        let _ = std::thread::spawn(move || {
            let _g = m2.lock().unwrap();
            panic!("poison it");
        })
        .join();
        assert_eq!(*m.lock_recover(), 7);
    }

    #[test]
    fn rwlock_recovers_from_poison() {
        let r = Arc::new(RwLock::new(7i32));
        let r2 = r.clone();
        let _ = std::thread::spawn(move || {
            let mut g = r2.write().unwrap();
            *g = 8;
            panic!("poison it");
        })
        .join();
        assert_eq!(*r.read_recover(), 8);
        *r.write_recover() = 9;
        assert_eq!(*r.read_recover(), 9);
    }
}
