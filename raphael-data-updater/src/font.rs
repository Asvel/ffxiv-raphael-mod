use allsorts::binary::read::ReadScope;
use allsorts::font::read_cmap_subtable;
use allsorts::font_data::FontData;
use allsorts::subset;
use allsorts::tables::cmap::Cmap;
use allsorts::tables::FontTableProvider;
use allsorts::tag;

pub fn generate_font_subset(dst_font: &str, base_font: &str, path_or_texts: &[&str]) {
    let font_file = std::fs::read(base_font).unwrap();
    let font_data = ReadScope::new(&font_file).read::<FontData>().unwrap();
    let font_provider = font_data.table_provider(0).unwrap();

    let cmap_data = font_provider.read_table_data(tag::CMAP).unwrap();
    let cmap = ReadScope::new(&cmap_data).read::<Cmap>().unwrap();
    let (_, cmap_subtable) = read_cmap_subtable(&cmap).unwrap().unwrap();

    let mut usages = [false; 0x10000];
    for &path_or_text in path_or_texts {
        let content;
        let chars = if path_or_text.starts_with("./") {
            content = std::fs::read_to_string(path_or_text).unwrap();
            content.chars()
        } else {
            path_or_text.chars()
        };
        for ch in chars {
            let ch = ch as usize;
            if 0xFF < ch && ch < usages.len() {
                usages[ch] = true;
            }
        }
    }

    let mut glyph_ids = vec![0];
    for (ch, used) in usages.into_iter().enumerate() {
        if used && let Ok(Some(glyph_id)) = cmap_subtable.map_glyph(ch as u32) {
            glyph_ids.push(glyph_id);
        }
    }
    glyph_ids.sort();
    glyph_ids.dedup();

    let font_subset = subset::subset(&font_provider, &glyph_ids).unwrap();
    std::fs::write(dst_font, &font_subset).unwrap();
}
