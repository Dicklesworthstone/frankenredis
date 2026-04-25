use fr_store::Store;
use proptest::prelude::*;

fn fresh_store() -> Store {
    Store::new()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    // MR1: BITOP AND with self is identity
    #[test]
    fn mr_bitop_and_identity(
        key in prop::collection::vec(any::<u8>(), 1..16),
        dest in prop::collection::vec(any::<u8>(), 1..16),
        value in prop::collection::vec(any::<u8>(), 1..256)
    ) {
        prop_assume!(key != dest);
        let mut store = fresh_store();
        store.set(key.clone(), value.clone(), None, 0);

        let len = store.bitop(b"AND", &dest, &[key.as_slice()], 0).unwrap();
        prop_assert_eq!(len, value.len());

        let retrieved = store.get(&dest, 0).unwrap();
        prop_assert_eq!(retrieved, Some(value));
    }

    // MR2: BITOP OR with self is identity
    #[test]
    fn mr_bitop_or_identity(
        key in prop::collection::vec(any::<u8>(), 1..16),
        dest in prop::collection::vec(any::<u8>(), 1..16),
        value in prop::collection::vec(any::<u8>(), 1..256)
    ) {
        prop_assume!(key != dest);
        let mut store = fresh_store();
        store.set(key.clone(), value.clone(), None, 0);

        let len = store.bitop(b"OR", &dest, &[key.as_slice()], 0).unwrap();
        prop_assert_eq!(len, value.len());

        let retrieved = store.get(&dest, 0).unwrap();
        prop_assert_eq!(retrieved, Some(value));
    }

    // MR3: BITOP XOR with self is all zeros
    #[test]
    fn mr_bitop_xor_self_zeros(
        key in prop::collection::vec(any::<u8>(), 1..16),
        dest in prop::collection::vec(any::<u8>(), 1..16),
        value in prop::collection::vec(any::<u8>(), 1..256)
    ) {
        prop_assume!(key != dest);
        let mut store = fresh_store();
        store.set(key.clone(), value.clone(), None, 0);

        let len = store.bitop(b"XOR", &dest, &[key.as_slice(), key.as_slice()], 0).unwrap();
        prop_assert_eq!(len, value.len());

        let retrieved = store.get(&dest, 0).unwrap().unwrap();
        let expected = vec![0u8; value.len()];
        prop_assert_eq!(retrieved, expected);
    }

    // MR4: BITOP AND is commutative
    #[test]
    fn mr_bitop_and_commutative(
        key_a in prop::collection::vec(any::<u8>(), 1..16),
        key_b in prop::collection::vec(any::<u8>(), 1..16),
        dest1 in prop::collection::vec(any::<u8>(), 1..16),
        dest2 in prop::collection::vec(any::<u8>(), 1..16),
        val_a in prop::collection::vec(any::<u8>(), 1..256),
        val_b in prop::collection::vec(any::<u8>(), 1..256)
    ) {
        prop_assume!(key_a != key_b && dest1 != dest2 && dest1 != key_a && dest1 != key_b && dest2 != key_a && dest2 != key_b);

        let mut store1 = fresh_store();
        store1.set(key_a.clone(), val_a.clone(), None, 0);
        store1.set(key_b.clone(), val_b.clone(), None, 0);
        store1.bitop(b"AND", &dest1, &[key_a.as_slice(), key_b.as_slice()], 0).unwrap();

        let mut store2 = fresh_store();
        store2.set(key_a.clone(), val_a.clone(), None, 0);
        store2.set(key_b.clone(), val_b.clone(), None, 0);
        store2.bitop(b"AND", &dest2, &[key_b.as_slice(), key_a.as_slice()], 0).unwrap();

        let res1 = store1.get(&dest1, 0).unwrap();
        let res2 = store2.get(&dest2, 0).unwrap();
        prop_assert_eq!(res1, res2);
    }

    // MR5: BITOP OR is commutative
    #[test]
    fn mr_bitop_or_commutative(
        key_a in prop::collection::vec(any::<u8>(), 1..16),
        key_b in prop::collection::vec(any::<u8>(), 1..16),
        dest1 in prop::collection::vec(any::<u8>(), 1..16),
        dest2 in prop::collection::vec(any::<u8>(), 1..16),
        val_a in prop::collection::vec(any::<u8>(), 1..256),
        val_b in prop::collection::vec(any::<u8>(), 1..256)
    ) {
        prop_assume!(key_a != key_b && dest1 != dest2 && dest1 != key_a && dest1 != key_b && dest2 != key_a && dest2 != key_b);

        let mut store1 = fresh_store();
        store1.set(key_a.clone(), val_a.clone(), None, 0);
        store1.set(key_b.clone(), val_b.clone(), None, 0);
        store1.bitop(b"OR", &dest1, &[key_a.as_slice(), key_b.as_slice()], 0).unwrap();

        let mut store2 = fresh_store();
        store2.set(key_a.clone(), val_a.clone(), None, 0);
        store2.set(key_b.clone(), val_b.clone(), None, 0);
        store2.bitop(b"OR", &dest2, &[key_b.as_slice(), key_a.as_slice()], 0).unwrap();

        let res1 = store1.get(&dest1, 0).unwrap();
        let res2 = store2.get(&dest2, 0).unwrap();
        prop_assert_eq!(res1, res2);
    }

    // MR6: BITOP NOT twice is identity
    #[test]
    fn mr_bitop_not_invertive(
        key in prop::collection::vec(any::<u8>(), 1..16),
        dest1 in prop::collection::vec(any::<u8>(), 1..16),
        dest2 in prop::collection::vec(any::<u8>(), 1..16),
        value in prop::collection::vec(any::<u8>(), 1..256)
    ) {
        prop_assume!(key != dest1 && key != dest2 && dest1 != dest2);
        let mut store = fresh_store();
        store.set(key.clone(), value.clone(), None, 0);

        store.bitop(b"NOT", &dest1, &[key.as_slice()], 0).unwrap();
        store.bitop(b"NOT", &dest2, &[dest1.as_slice()], 0).unwrap();

        let retrieved = store.get(&dest2, 0).unwrap();
        prop_assert_eq!(retrieved, Some(value));
    }

    // MR7: BITFIELD SET then GET recovers same value (Identity)
    #[test]
    fn mr_bitfield_set_get_identity(
        key in prop::collection::vec(any::<u8>(), 1..16),
        offset in 0u64..1000,
        bits in 1u8..64,
        value in any::<i64>(),
        signed in any::<bool>()
    ) {
        let mut store = fresh_store();
        // Mask value to fit in `bits` wide field
        let mask = if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 };
        let masked_val = (value as u64 & mask) as i64;

        store.bitfield_set(&key, offset, bits, masked_val, 0).unwrap();

        let retrieved = store.bitfield_get(&key, offset, bits, signed, 0).unwrap();

        if signed {
            // Re-sign-extend for comparison
            let shift = 64 - bits;
            let expected_signed = (masked_val << shift) >> shift;
            prop_assert_eq!(retrieved, expected_signed);
        } else {
            prop_assert_eq!(retrieved, masked_val);
        }
    }
}
