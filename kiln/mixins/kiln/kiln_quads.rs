// Extra quads that a Java mod adds to a block's faces (Weather Glass's drops on glass), kept for the mesher.
//
// Java (Fabric's `WrapperBlockStateModel` / `ModelModifier.AfterBakeBlock`) emits quads for a block state; the
// game's section mesher asks here, face by face, for the ones that belong to the face it has just drawn, and
// draws them the way it draws an overlay (`kiln_extra_quads` in mesher_quads.rs). The store lives in the render
// crate: the game's own crate fills it, the mesher (worker threads) reads it.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

/// One quad, in the block's own coordinates (0..1), over the face it was emitted for.
#[derive(Clone, Debug, PartialEq)]
pub struct ExtraQuad {
    pub positions: [[f32; 3]; 4],
    /// Sprite-local UVs (0..1), as the mesher's overlays use them.
    pub uv: [[f32; 2]; 4],
    /// The block sprite's name (`weatherglass:block/...`), told to a texture layer by the game's sprite table.
    pub sprite: String,
    /// Colour multiplied into the sprite, 0xRRGGBB.
    pub color: u32,
    /// Drawn in the translucent pass (raindrops are).
    pub translucent: bool,
}

type Store = HashMap<(u32, u8), Arc<[ExtraQuad]>>;

static ANY: AtomicBool = AtomicBool::new(false);
static STORE: RwLock<Option<Store>> = RwLock::new(None);

/// Whether any block has extra quads: the mesher's one cheap test.
pub fn any() -> bool {
    ANY.load(Ordering::Relaxed)
}

/// The extra quads of block state `state` on face `facing` (the model direction's number).
pub fn get(state: u32, facing: u8) -> Option<Arc<[ExtraQuad]>> {
    STORE.read().ok()?.as_ref()?.get(&(state, facing)).cloned()
}

/// Replaces the quads of one block state's face (none removes them).
pub fn set(state: u32, facing: u8, quads: Vec<ExtraQuad>) {
    if let Ok(mut store) = STORE.write() {
        let map = store.get_or_insert_with(HashMap::new);
        if quads.is_empty() {
            map.remove(&(state, facing));
        } else {
            map.insert((state, facing), quads.into());
        }
        ANY.store(!map.is_empty(), Ordering::Relaxed);
    }
}

/// Removes every extra quad (a world closed, the mods stopped).
pub fn clear() {
    if let Ok(mut store) = STORE.write() {
        *store = None;
    }
    ANY.store(false, Ordering::Relaxed);
}
