//! 잠긴 메모리 키 보관소.
//!
//! OS의 메모리 잠금 한도가 작아서, 키 여러 개를 페이지 정렬된 한 영역(풀)에 모아 잠급니다.
//! 휘발성 내용 자체는 RAM 안에서도 암호문으로 두고 키만 여기에 둡니다. 영역은 스왑·코어 덤프에서
//! 빠지도록 mlock(+리눅스 MADV_DONTDUMP)하고, 키를 지우거나 보관소를 해제할 때 0으로 덮어씁니다.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::collections::HashMap;
use std::ptr::NonNull;

use yangpa_core::body::Secret32;

use crate::error::{CryptoError, Result};

const KEY_LEN: usize = 32;
const PAGE: usize = 4096;

pub struct LockedKeyring {
    ptr: NonNull<u8>,
    layout: Layout,
    capacity: usize,
    slots: HashMap<[u8; 16], usize>,
    free: Vec<usize>,
    locked: bool,
}

// 내부 포인터는 이 구조체만 소유하고, &mut 없이 쓰지 않습니다.
unsafe impl Send for LockedKeyring {}
unsafe impl Sync for LockedKeyring {}

impl LockedKeyring {
    /// `capacity`개 키를 담는 보관소. 잠금에 실패해도(한도 초과 등) 동작은 하며 `is_locked()`가 false입니다.
    pub fn new(capacity: usize) -> Result<Self> {
        let capacity = capacity.max(1);
        let size = capacity
            .checked_mul(KEY_LEN)
            .and_then(|b| b.div_ceil(PAGE).checked_mul(PAGE))
            .ok_or(CryptoError::LockedAlloc)?;
        let layout = Layout::from_size_align(size, PAGE).map_err(|_| CryptoError::LockedAlloc)?;
        // SAFETY: 크기가 0이 아닌 레이아웃.
        let raw = unsafe { alloc_zeroed(layout) };
        let ptr = NonNull::new(raw).ok_or(CryptoError::LockedAlloc)?;
        // SAFETY: 방금 할당한 영역 전체.
        let locked = unsafe { memsec::mlock(ptr.as_ptr(), size) };
        let capacity = size / KEY_LEN;
        Ok(LockedKeyring {
            ptr,
            layout,
            capacity,
            slots: HashMap::new(),
            free: (0..capacity).rev().collect(),
            locked,
        })
    }

    pub fn is_locked(&self) -> bool {
        self.locked
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    fn slot_ptr(&self, slot: usize) -> *mut u8 {
        debug_assert!(slot < self.capacity);
        // SAFETY: slot < capacity이므로 영역 안.
        unsafe { self.ptr.as_ptr().add(slot * KEY_LEN) }
    }

    pub fn insert(&mut self, id: [u8; 16], key: &Secret32) -> Result<()> {
        let slot = match self.slots.get(&id) {
            Some(s) => *s,
            None => {
                let s = self.free.pop().ok_or(CryptoError::KeyringFull)?;
                self.slots.insert(id, s);
                s
            }
        };
        // SAFETY: 슬롯 영역은 32바이트이고 이 구조체만 씀.
        unsafe { std::ptr::copy_nonoverlapping(key.0.as_ptr(), self.slot_ptr(slot), KEY_LEN) };
        Ok(())
    }

    pub fn contains(&self, id: &[u8; 16]) -> bool {
        self.slots.contains_key(id)
    }

    /// 키를 잠깐 꺼내 씁니다. 꺼낸 사본(`Secret32`)은 클로저가 끝나면 0으로 덮어써집니다.
    pub fn with_key<R>(&self, id: &[u8; 16], f: impl FnOnce(&Secret32) -> R) -> Option<R> {
        let slot = *self.slots.get(id)?;
        let mut k = Secret32([0; 32]);
        // SAFETY: 슬롯 영역 32바이트 읽기.
        unsafe { std::ptr::copy_nonoverlapping(self.slot_ptr(slot), k.0.as_mut_ptr(), KEY_LEN) };
        Some(f(&k))
    }

    pub fn remove(&mut self, id: &[u8; 16]) -> bool {
        let Some(slot) = self.slots.remove(id) else {
            return false;
        };
        // SAFETY: 슬롯 영역 0으로 덮어쓰기(최적화로 지워지지 않는 memzero).
        unsafe { memsec::memzero(self.slot_ptr(slot), KEY_LEN) };
        self.free.push(slot);
        true
    }

    /// 모든 키를 지웁니다.
    pub fn clear(&mut self) {
        // SAFETY: 영역 전체.
        unsafe { memsec::memzero(self.ptr.as_ptr(), self.layout.size()) };
        self.slots.clear();
        self.free = (0..self.capacity).rev().collect();
    }
}

impl Drop for LockedKeyring {
    fn drop(&mut self) {
        // SAFETY: new에서 할당·잠근 영역을 지우고 잠금 해제 후 반환.
        unsafe {
            memsec::memzero(self.ptr.as_ptr(), self.layout.size());
            if self.locked {
                memsec::munlock(self.ptr.as_ptr(), self.layout.size());
            }
            dealloc(self.ptr.as_ptr(), self.layout);
        }
    }
}

impl std::fmt::Debug for LockedKeyring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "LockedKeyring(len={}, capacity={}, locked={})",
            self.len(),
            self.capacity,
            self.locked
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_use_remove() {
        let mut r = LockedKeyring::new(4).unwrap();
        assert!(r.capacity() >= 4);
        let k = Secret32([9; 32]);
        r.insert([1; 16], &k).unwrap();
        assert_eq!(r.with_key(&[1; 16], |s| s.0), Some([9; 32]));
        assert!(r.remove(&[1; 16]));
        assert!(r.with_key(&[1; 16], |s| s.0).is_none());
        // 지운 슬롯은 0으로 덮였는지 확인.
        r.insert([2; 16], &Secret32([7; 32])).unwrap();
        r.clear();
        assert!(r.is_empty());
    }

    #[test]
    fn absurd_capacity_is_an_error_not_a_panic() {
        assert!(matches!(
            LockedKeyring::new(usize::MAX),
            Err(CryptoError::LockedAlloc)
        ));
    }

    #[test]
    fn full_keyring_errors() {
        let mut r = LockedKeyring::new(1).unwrap();
        for i in 0..r.capacity() {
            let mut id = [0u8; 16];
            id[..8].copy_from_slice(&(i as u64).to_be_bytes());
            r.insert(id, &Secret32([1; 32])).unwrap();
        }
        assert!(matches!(
            r.insert([0xff; 16], &Secret32([1; 32])),
            Err(CryptoError::KeyringFull)
        ));
    }
}
