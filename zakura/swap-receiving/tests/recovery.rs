use incrementalmerkletree::Hashable;
use orchard::{
    keys::{FullViewingKey, Scope, SpendingKey},
    note::{Note, NoteVersion, RandomSeed, Rho},
    note_encryption::{IronwoodDomain, IronwoodNoteEncryption},
    tree::{Anchor, MerkleHashOrchard, MerklePath},
    value::NoteValue,
};
use zakura_swap_receiving::{
    KeyId, Purpose,
    recovery::{EncryptedNote, RecoveryError},
};
use zcash_note_encryption::Domain;

#[test]
fn authenticate_derived_key_and_bind_position() {
    let account = FullViewingKey::from(&SpendingKey::from_bytes([0; 32]).unwrap());
    for purpose in [Purpose::Refund, Purpose::Receive] {
        let id = KeyId::new(purpose, 7);
        let key = id.derive(&account).unwrap();
        let rho = Rho::from_bytes(&[9; 32]).unwrap();
        let note = Note::from_parts(
            key.address_at(0u32, Scope::External),
            NoteValue::from_raw(30_000),
            rho,
            RandomSeed::from_bytes([7; 32], &rho).unwrap(),
            NoteVersion::V3,
        )
        .unwrap();
        let encryptor = IronwoodNoteEncryption::new(None, note, [4; 512]);
        let bytes = encryptor.encrypt_note_plaintext();
        let encrypted = EncryptedNote::from_parts(
            [9; 32],
            orchard::note::ExtractedNoteCommitment::from(note.commitment()).to_bytes(),
            IronwoodDomain::epk_bytes(encryptor.epk()).0,
            bytes[..52].try_into().unwrap(),
            bytes[52..].try_into().unwrap(),
        );
        let restored = EncryptedNote::from_bytes(*encrypted.to_bytes());
        let recovered = restored.decrypt(&account, id).unwrap();
        assert_eq!(recovered.note(), &note);
        assert_eq!(recovered.memo(), &[4; 512]);
        assert_eq!(recovered.nullifier(), &note.nullifier(&key));
        assert_eq!(recovered.key_id(), id);
        assert!(restored.decrypt(&account, KeyId::new(purpose, 8)).is_err());
        let other = FullViewingKey::from(&SpendingKey::from_bytes([1; 32]).unwrap());
        assert!(restored.decrypt(&other, id).is_err());
        for offset in [0, 32, 64, 96, 148, 675] {
            let mut bytes = *restored.to_bytes();
            bytes[offset] ^= 1;
            assert!(
                EncryptedNote::from_bytes(bytes)
                    .decrypt(&account, id)
                    .is_err()
            );
        }
        let path = MerklePath::from_parts(
            2,
            std::array::from_fn(|i| MerkleHashOrchard::empty_root((i as u8).into())),
        );
        let root = path.root(note.commitment().into());
        recovered.verify_position(2, &path, root).unwrap();
        assert_eq!(
            recovered.verify_position(3, &path, root),
            Err(RecoveryError::Witness)
        );
        assert_eq!(
            recovered.verify_position((1u64 << 32) + 2, &path, root),
            Err(RecoveryError::Witness)
        );
        assert_eq!(
            recovered.verify_position(2, &path, Anchor::empty_tree()),
            Err(RecoveryError::Witness)
        );
    }
}
