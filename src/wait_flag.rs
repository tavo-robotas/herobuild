use async_std::sync::{Condvar, Mutex};

#[derive(Default, Debug)]
pub struct WaitFlag<T> {
    mutex: Mutex<T>,
    cond: Condvar,
}

impl<T: Copy + Eq> WaitFlag<T> {
    pub fn new(value: T) -> Self {
        Self {
            mutex: Mutex::new(value),
            cond: Condvar::default(),
        }
    }

    pub async fn set(&self, value: T) {
        let mut guard = self.mutex.lock().await;
        *guard = value;
        self.cond.notify_all();
    }

    pub async fn get(&self) -> T {
        *self.mutex.lock().await
    }

    pub async fn modify(&self, func: impl FnOnce(&mut T)) {
        let mut guard = self.mutex.lock().await;
        func(&mut *guard);
        self.cond.notify_all();
    }

    pub async fn wait(&self, value: T) {
        self.cond
            .wait_until(self.mutex.lock().await, |flag| *flag == value)
            .await;
    }
}
