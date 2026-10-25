//! Document font selection and contextual glyph save/reopen acceptance on synthetic PDFs.
use crate::support;
use pdfcraft_cos::{Dict, Document, Object, SaveOptions, Stream};
use pdfcraft_engine::{BlockStyle, Edit, Session};
use pdfcraft_render::{PageRenderer, RenderRequest};
use std::sync::Arc;
fn document() -> Arc<Vec<u8>> {
    let mut d = Document::new_empty();
    let pages = d.add(Object::Dict(Dict::new()));
    let contents = d.add(Object::Stream(Stream::flate(Dict::new(), b"BT /F1 18 Tf 30 160 Td (Source) Tj ET")));
    let mut font = Dict::new();
    font.set(b"Type".to_vec(), Object::name("Font"));
    font.set(b"Subtype".to_vec(), Object::name("Type1"));
    font.set(b"BaseFont".to_vec(), Object::name("Helvetica"));
    let mut fonts = Dict::new();
    fonts.set(b"F1".to_vec(), Object::Dict(font));
    let mut res = Dict::new();
    res.set(b"Font".to_vec(), Object::Dict(fonts));
    let mut p = Dict::new();
    p.set(b"Type".to_vec(), Object::name("Page"));
    p.set(b"Parent".to_vec(), Object::Ref(pages));
    p.set(b"Contents".to_vec(), Object::Ref(contents));
    p.set(b"Resources".to_vec(), Object::Dict(res));
    p.set(b"MediaBox".to_vec(), Object::Array([0, 0, 300, 240].map(Object::Int).to_vec()));
    let page = d.add(Object::Dict(p));
    d.update_dict(pages, |p| {
        p.set(b"Type".to_vec(), Object::name("Pages"));
        p.set(b"Kids".to_vec(), Object::Array(vec![Object::Ref(page)]));
        p.set(b"Count".to_vec(), Object::Int(1));
    })
    .unwrap();
    let mut root = Dict::new();
    root.set(b"Type".to_vec(), Object::name("Catalog"));
    root.set(b"Pages".to_vec(), Object::Ref(pages));
    let root = d.add(Object::Dict(root));
    d.trailer_mut().set(b"Root".to_vec(), Object::Ref(root));
    Arc::new(pdfcraft_cos::write_full(&d, &SaveOptions::default()).unwrap())
}
fn pixels(bytes: Arc<Vec<u8>>) -> Vec<u8> {
    let mut renderer = PageRenderer::new(bytes, Default::default());
    let p = renderer.render(RenderRequest { page: 0, scale: 2.0, ..Default::default() });
    assert!(p.error.is_none(), "{:?}", p.error);
    assert!(p.warnings.is_empty(), "{:?}", p.warnings);
    p.rgba.to_vec()
}
fn exercise(font: Vec<u8>, text: &str, expected_family: &str) {
    let mut s = Session::new();
    let id = s.open("source.pdf", None, document(), None).unwrap();
    let original = pixels(s.get(id).unwrap().display.clone());
    s.apply(id, Edit::RegisterTextFont { page: 0, bytes: Arc::new(font.clone()) }).unwrap();
    let resource = s.get(id).unwrap().text_editor_fonts(0).into_iter().find(|f| f.bytes == font).unwrap().resource;
    let style = BlockStyle { resource: Some(resource), width: Some(240.0), ..Default::default() };
    s.apply(id, Edit::EditTextBlock { page: 0, block: 0, text: text.into(), style: style.clone() }).unwrap();
    let edited = s.get(id).unwrap();
    let lines = edited.text_lines(0);
    assert_eq!(lines.iter().map(|l| l.text.as_str()).collect::<String>(), text);
    assert!(lines.iter().all(|l| l.base_font == expected_family));
    assert!((lines[0].baseline()[1] - 160.0).abs() < 0.01);
    let current = pixels(edited.display.clone());
    assert_ne!(current, original);
    let saved = s.save_bytes(id).unwrap();
    let re = s.open("saved.pdf", None, saved, None).unwrap();
    assert_eq!(pixels(s.get(re).unwrap().display.clone()), current);
    assert_eq!(s.get(re).unwrap().text_blocks(0)[0].editor_text, text);
    let twice = format!("{text} {text}");
    s.apply(re, Edit::EditTextBlock { page: 0, block: 0, text: twice.clone(), style: style.clone() }).unwrap();
    assert_eq!(s.get(re).unwrap().text_blocks(0)[0].editor_text, twice);
    let again = s.save_bytes(re).unwrap();
    let next = s.open("twice.pdf", None, again, None).unwrap();
    assert_eq!(s.get(next).unwrap().text_blocks(0)[0].editor_text, twice);
    assert_eq!(pixels(s.get(next).unwrap().display.clone()), pixels(s.get(re).unwrap().display.clone()));
    s.apply(re, Edit::EditTextBlock { page: 0, block: 0, text: String::new(), style }).unwrap();
    assert!(s.get(re).unwrap().text_lines(0).is_empty(), "every contextual glyph is deleted");
    let empty = pixels(s.get(re).unwrap().display.clone());
    assert!(empty.as_chunks::<4>().0.iter().all(|p| p[..3] == [255; 3]));
    s.undo(id).unwrap();
    assert_eq!(pixels(s.get(id).unwrap().display.clone()), original);
}
#[test]
fn opentype_cff_document_font_preserves_program_codes_and_rendering_after_reopen() {
    exercise(support::font_fixture::cff_font(), "A A", "RectFont-Regular");
}
#[test]
fn contextual_arabic_marks_ligatures_and_mixed_bidi_keep_logical_text_after_reopen() {
    if let Some(path) = std::env::var_os("PDFCRAFT_TEST_ARABIC_FONT") {
        exercise(std::fs::read(path).unwrap(), "سَلَام 123", "NotoSansArabic-Regular");
        return;
    }
    let Some(font) = pdfcraft_fonts::document_arabic_font() else {
        eprintln!("set CRAFT_FONTS_DIR to test full Arabic glyph programs");
        return;
    };
    if pdfcraft_fonts::embedding::validate(font.bytes).is_err() {
        eprintln!("set PDFCRAFT_TEST_ARABIC_FONT to an openly licensed static Noto Sans Arabic face");
        return;
    }
    exercise(font.bytes.to_vec(), "سَلَام 123", "NotoSansArabic-Regular");
}

#[test]
fn added_font_is_subset_to_used_glyphs_on_save() {
    let font_bytes = std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/fonts/Inter-Regular.ttf")).unwrap();
    let font_len = font_bytes.len();
    assert!(font_len > 300_000, "source font is full sized");

    let mut s = Session::new();
    let id = s.open("source.pdf", None, document(), None).unwrap();
    s.apply(id, Edit::RegisterTextFont { page: 0, bytes: Arc::new(font_bytes) }).unwrap();
    let resource = s.get(id).unwrap().text_editor_fonts(0).into_iter().find(|f| f.face == "Inter-Regular").unwrap().resource;
    let style = BlockStyle { resource: Some(resource), width: Some(240.0), ..Default::default() };
    let text = "Three words here";
    s.apply(id, Edit::EditTextBlock { page: 0, block: 0, text: text.into(), style }).unwrap();

    let saved = s.save_bytes(id).unwrap();
    assert!(saved.len() < 50_000, "saved PDF with 3 words must be subset and small, was {} bytes", saved.len());

    let saved_doc = pdfcraft_cos::Document::open(saved.clone()).unwrap();
    let mut subset_found = false;
    for (_, obj) in saved_doc.scan_objects() {
        if let pdfcraft_cos::Object::Dict(d) = obj.as_ref()
            && d.name(b"Type") == Some(b"Font")
            && d.name(b"Subtype") == Some(b"Type0")
            && let Some(base) = d.name(b"BaseFont")
        {
            let base_str = String::from_utf8_lossy(base);
            if base_str.ends_with("Inter-Regular") {
                assert!(base_str.len() >= 7 && base_str.as_bytes()[6] == b'+', "font base name {base_str} must have subset prefix");
                subset_found = true;
            }
        }
        if let pdfcraft_cos::Object::Stream(stream) = obj.as_ref()
            && stream.dict.get(b"PCFullFont").is_some()
        {
            panic!("saved PDF must not retain PCFullFont flag");
        }
    }
    assert!(subset_found, "Type0 subset font must be present in saved document");

    // Also verify reopening and rendering matches:
    let re = s.open("saved.pdf", None, saved, None).unwrap();
    assert_eq!(s.get(re).unwrap().text_blocks(0)[0].editor_text, text);
    let re_pixels = pixels(s.get(re).unwrap().display.clone());
    let orig_pixels = pixels(s.get(id).unwrap().display.clone());
    assert_eq!(re_pixels, orig_pixels, "reopened subset font renders identically to original");
}
