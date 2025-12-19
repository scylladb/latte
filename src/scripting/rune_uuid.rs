use metrohash::MetroHash128;
use rune::alloc::fmt::TryWrite;
use rune::runtime::VmResult;
use rune::{vm_write, Any};
use std::hash::Hash;
use uuid::{Variant, Version};

#[derive(Clone, Debug, Any)]
pub struct Uuid(pub uuid::Uuid);

impl Uuid {
    pub fn new(i: i64) -> Uuid {
        let mut hash = MetroHash128::new();
        i.hash(&mut hash);
        let (h1, h2) = hash.finish128();
        let h = ((h1 as u128) << 64) | (h2 as u128);
        let mut builder = uuid::Builder::from_u128(h);
        builder.set_variant(Variant::RFC4122);
        builder.set_version(Version::Random);
        Uuid(builder.into_uuid())
    }

    #[rune::function(protocol = STRING_DISPLAY)]
    pub fn string_display(&self, f: &mut rune::runtime::Formatter) -> VmResult<()> {
        vm_write!(f, "{}", self.0);
        VmResult::Ok(())
    }
}
