// The start of `Mesher::face`: the extra quads Java mods gave this face, drawn with it as an overlay is.
fn kiln_extra_quads(
    mesher: &Mesher<'_>,
    pad: &Padded,
    out: &mut SectionMesh,
    pos: [i32; 3],
    origin: [f32; 3],
    face: &BakedFace,
) {
    if !crate::kiln_quads::any() {
        return;
    }
    let id = pad.blocks[pi(pos[0], pos[1], pos[2])];
    let Some(quads) = crate::kiln_quads::get(id, face.quad.facing as u8) else {
        return;
    };
    let state = &mesher.tables.info[id as usize];
    let coords = pack_coords(lighting::flat(
        pad,
        state,
        pos,
        &face.quad.positions,
        face.quad.facing,
    ));
    for quad in quads.iter() {
        let Some(&layer) = mesher.render.sprite_layers.get(quad.sprite.as_str()) else {
            continue;
        };
        let vertices = std::array::from_fn(|i| {
            let p = quad.positions[i];
            TerrainVertex::new(
                [p[0] + origin[0], p[1] + origin[1], p[2] + origin[2]],
                quad.uv[i],
                0,
                quad.color,
                coords,
            )
        });
        let tile = TileLayer::Layer {
            layer: layer as u16,
            translucent: quad.translucent,
            opaque: false,
        };
        mesher.quad_as(out, false, tile, vertices, false, Some(quad.color));
    }
}
