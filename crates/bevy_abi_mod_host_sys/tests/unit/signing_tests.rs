use super::*;
use bevy_ecs::world::World;
use ssh_key::{Algorithm, PrivateKey, rand_core::OsRng};

#[test]
fn host_sys_trust_ecs_adapters_store_and_verify() {
    let key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
    let pub_str = key.public_key().to_openssh().unwrap();
    let policy = ModTrustPolicy::from_public_key_openssh(&pub_str).unwrap();
    let payload = b"host-sys-adapter-payload";
    let sig = sign_mod_bytes_with_key(&key, DEFAULT_SIGNATURE_NAMESPACE, payload).unwrap();
    let receipt = policy.verify_bytes(payload, sig.as_bytes()).unwrap();
    policy.verify_receipt_still_trusted(&receipt).unwrap();

    let mut world = World::new();
    world.insert_resource(policy);
    let entity = world.spawn(receipt.clone()).id();
    assert_eq!(world.get::<VerifiedModSignature>(entity), Some(&receipt));
    assert!(world.resource::<ModTrustPolicy>().has_trust_anchors());
}
