use blitz_traits::node_id::NodeId;
use selectors::context::QuirksMode;
use std::sync::atomic::Ordering as Ao;
use std::{
    io::Cursor,
    sync::{Arc, atomic::AtomicUsize, mpsc::Sender},
};
use style::{
    font_face::{FontFaceSourceFormat, FontFaceSourceFormatKeyword, FontStyleRange, Source},
    media_queries::MediaList,
    servo_arc::Arc as ServoArc,
    shared_lock::SharedRwLock,
    shared_lock::{Locked, SharedRwLockReadGuard},
    stylesheets::{
        AllowImportRules, CssRule, DocumentStyleSheet, ImportRule, Origin, Stylesheet,
        StylesheetInDocument, StylesheetLoader as ServoStylesheetLoader, UrlExtraData,
        import_rule::{ImportLayer, ImportSheet, ImportSupportsCondition},
    },
    values::{CssUrl, SourceLocation},
};

use blitz_traits::net::http::{HeaderName, HeaderValue};
use blitz_traits::net::{AbortSignal, Bytes, NetHandler, NetProvider, Request};
use blitz_traits::shell::ShellProvider;

use url::Url;

use crate::{document::DocumentEvent, util::ImageType};

pub(crate) fn stamped_request(url: Url, signal: Option<&AbortSignal>) -> Request {
    let mut req = Request::get(url);
    if let Some(sig) = signal {
        req = req.signal(sig.clone());
    }
    req
}

/// Carries `@font-face` descriptors from CSS parsing through to font
/// registration so `parley::Collection::register_fonts` can alias the bytes
/// under the `font-family` declared in CSS rather than whatever family name
/// the TTF's own `name` table reports.
///
/// All fields are `Option` because each descriptor is independently optional
/// at the CSS level. Missing fields fall back to the values parley reads
/// from the font's own metadata.
#[derive(Clone, Debug, Default)]
pub struct FontFaceOverrides {
    /// `font-family` descriptor (the alias the rest of the stylesheet uses).
    pub family_name: Option<String>,
    /// `font-weight` descriptor as a single CSS weight (100–900). Stylo
    /// parses this as a range; we record the lower bound, which equals the
    /// upper bound in the common single-value case.
    pub weight: Option<f32>,
    /// `font-style` descriptor mapped to fontique's `FontStyle`.
    pub style: Option<parley::fontique::FontStyle>,
}

#[derive(Clone, Debug)]
pub enum Resource {
    /// RGBA8 pixels already turned by the EXIF orientation that follows them.
    Image(
        ImageType,
        u32,
        u32,
        Arc<Vec<u8>>,
        image::metadata::Orientation,
    ),
    #[cfg(feature = "svg")]
    Svg(ImageType, crate::node::SvgImageData),
    Css(DocumentStyleSheet),
    Font(Bytes, FontFaceOverrides),
    /// HTML fetched for an `<iframe>` element's `src`
    DocumentSrc(String),
    /// An `@import`ed stylesheet that was parsed straight into its import rule
    ImportedStylesheet,
    None,
}

pub(crate) struct ResourceHandler<T: Send + Sync + 'static> {
    doc_id: usize,
    request_id: usize,
    node_id: Option<NodeId>,
    tx: Sender<DocumentEvent>,
    shell_provider: Arc<dyn ShellProvider>,
    data: T,
}

impl<T: Send + Sync + 'static> ResourceHandler<T> {
    pub(crate) fn new(
        tx: Sender<DocumentEvent>,
        doc_id: usize,
        node_id: Option<NodeId>,
        shell_provider: Arc<dyn ShellProvider>,
        data: T,
    ) -> Self {
        static REQUEST_ID_COUNTER: AtomicUsize = AtomicUsize::new(0);
        Self {
            request_id: REQUEST_ID_COUNTER.fetch_add(1, Ao::Relaxed),
            doc_id,
            node_id,
            tx,
            shell_provider,
            data,
        }
    }

    pub(crate) fn boxed(
        tx: Sender<DocumentEvent>,
        doc_id: usize,
        node_id: Option<NodeId>,
        shell_provider: Arc<dyn ShellProvider>,
        data: T,
    ) -> Box<dyn NetHandler>
    where
        ResourceHandler<T>: NetHandler,
    {
        Box::new(Self::new(tx, doc_id, node_id, shell_provider, data)) as _
    }

    pub(crate) fn request_id(&self) -> usize {
        self.request_id
    }

    fn respond(&self, resolved_url: String, result: Result<Resource, String>) {
        let response = ResourceLoadResponse {
            request_id: self.request_id,
            node_id: self.node_id,
            resolved_url: Some(resolved_url),
            result,
        };
        let _ = self.tx.send(DocumentEvent::ResourceLoad(response));
        self.shell_provider.request_redraw();
    }
}

#[allow(unused)]
pub struct ResourceLoadResponse {
    pub request_id: usize,
    pub node_id: Option<NodeId>,
    pub resolved_url: Option<String>,
    pub result: Result<Resource, String>,
}

pub struct StylesheetHandler {
    pub source_url: Url,
    pub guard: SharedRwLock,
    pub net_provider: Arc<dyn NetProvider>,
    pub abort_signal: Option<AbortSignal>,
}

impl NetHandler for ResourceHandler<StylesheetHandler> {
    fn bytes(self: Box<Self>, resolved_url: String, bytes: Bytes) {
        let Ok(css) = std::str::from_utf8(&bytes) else {
            return self.respond(resolved_url, Err(String::from("Invalid UTF8")));
        };

        // NOTE(Nico): I don't *think* external stylesheets should have HTML entities escaped
        // let escaped_css = html_escape::decode_html_entities(css);

        let sheet = Stylesheet::from_str(
            css,
            self.data.source_url.clone().into(),
            Origin::Author,
            ServoArc::new(self.data.guard.wrap(MediaList::empty())),
            self.data.guard.clone(),
            Some(&StylesheetLoader {
                tx: self.tx.clone(),
                doc_id: self.doc_id,
                net_provider: self.data.net_provider.clone(),
                shell_provider: self.shell_provider.clone(),
                abort_signal: self.data.abort_signal.clone(),
            }),
            None, // error_reporter
            QuirksMode::NoQuirks,
            AllowImportRules::Yes,
        );

        self.respond(
            resolved_url,
            Ok(Resource::Css(DocumentStyleSheet(ServoArc::new(sheet)))),
        );
    }
}

#[derive(Clone)]
pub(crate) struct StylesheetLoader {
    pub(crate) tx: Sender<DocumentEvent>,
    pub(crate) doc_id: usize,
    pub(crate) net_provider: Arc<dyn NetProvider>,
    pub(crate) shell_provider: Arc<dyn ShellProvider>,
    pub(crate) abort_signal: Option<AbortSignal>,
}
impl ServoStylesheetLoader for StylesheetLoader {
    fn request_stylesheet(
        &self,
        url: CssUrl,
        location: SourceLocation,
        lock: &SharedRwLock,
        media: ServoArc<Locked<MediaList>>,
        supports: Option<ImportSupportsCondition>,
        layer: ImportLayer,
    ) -> ServoArc<Locked<ImportRule>> {
        let is_enabled = supports.as_ref().is_none_or(|s| s.enabled);
        // The page writes this url, and `url()` is `None` when it cannot parse.
        let Some(sheet_url) = url.url().filter(|_| is_enabled).cloned() else {
            #[cfg(feature = "tracing")]
            if is_enabled {
                tracing::warn!(
                    "Not loading @import: could not resolve url {}",
                    url.as_str()
                );
            }
            return ServoArc::new(lock.wrap(ImportRule {
                url,
                stylesheet: ImportSheet::new_refused(),
                supports,
                layer,
                source_location: location,
            }));
        };

        // `ImportRule::to_css` reads the media off the imported sheet, so until
        // the load lands an empty sheet carries it: a `Pending` import would
        // re-serialize without its media and lose it on the next re-parse.
        let placeholder = Stylesheet::from_str(
            "",
            UrlExtraData(sheet_url.clone()),
            Origin::Author,
            media.clone(),
            lock.clone(),
            None, // stylesheet_loader
            None, // error_reporter
            QuirksMode::NoQuirks,
            AllowImportRules::No,
        );
        let import = ImportRule {
            url,
            stylesheet: ImportSheet::new(ServoArc::new(placeholder)),
            supports,
            layer,
            source_location: location,
        };

        let url = sheet_url;
        let import = ServoArc::new(lock.wrap(import));
        self.net_provider.fetch(
            self.doc_id,
            stamped_request(url.as_ref().clone(), self.abort_signal.as_ref()),
            ResourceHandler::boxed(
                self.tx.clone(),
                self.doc_id,
                None, // node_id
                self.shell_provider.clone(),
                NestedStylesheetHandler {
                    url: url.clone(),
                    loader: self.clone(),
                    lock: lock.clone(),
                    media,
                    import_rule: import.clone(),
                    net_provider: self.net_provider.clone(),
                },
            ),
        );

        import
    }
}

struct NestedStylesheetHandler {
    loader: StylesheetLoader,
    lock: SharedRwLock,
    url: ServoArc<Url>,
    media: ServoArc<Locked<MediaList>>,
    import_rule: ServoArc<Locked<ImportRule>>,
    net_provider: Arc<dyn NetProvider>,
}

impl NetHandler for ResourceHandler<NestedStylesheetHandler> {
    fn bytes(self: Box<Self>, resolved_url: String, bytes: Bytes) {
        let Ok(css) = std::str::from_utf8(&bytes) else {
            return self.respond(resolved_url, Err(String::from("Invalid UTF8")));
        };

        // NOTE(Nico): I don't *think* external stylesheets should have HTML entities escaped
        // let escaped_css = html_escape::decode_html_entities(css);

        let sheet = ServoArc::new(Stylesheet::from_str(
            css,
            UrlExtraData(self.data.url.clone()),
            Origin::Author,
            self.data.media.clone(),
            self.data.lock.clone(),
            Some(&self.data.loader),
            None, // error_reporter
            QuirksMode::NoQuirks,
            AllowImportRules::Yes,
        ));

        // Fetch @font-face fonts
        fetch_font_face(
            self.tx.clone(),
            self.doc_id,
            self.node_id,
            &sheet,
            &self.data.net_provider,
            &self.shell_provider,
            &self.data.lock.read(),
            self.data.loader.abort_signal.as_ref(),
        );

        let mut guard = self.data.lock.write();
        self.data.import_rule.write_with(&mut guard).stylesheet = ImportSheet::Sheet(sheet);
        drop(guard);

        self.respond(resolved_url, Ok(Resource::ImportedStylesheet))
    }
}

struct FontFaceHandler {
    format: FontFaceSourceFormatKeyword,
    overrides: FontFaceOverrides,
}
impl NetHandler for ResourceHandler<FontFaceHandler> {
    fn bytes(mut self: Box<Self>, resolved_url: String, bytes: Bytes) {
        let result = self.data.parse(bytes);
        self.respond(resolved_url, result)
    }
}
impl FontFaceHandler {
    fn parse(&mut self, bytes: Bytes) -> Result<Resource, String> {
        if self.format == FontFaceSourceFormatKeyword::None && bytes.len() >= 4 {
            self.format = match &bytes.as_ref()[0..4] {
                // WOFF (v1) files begin with 0x774F4646 ('wOFF' in ascii)
                // See: <https://w3c.github.io/woff/woff1/spec/Overview.html#WOFFHeader>
                #[cfg(feature = "woff")]
                b"wOFF" => FontFaceSourceFormatKeyword::Woff,
                // WOFF2 files begin with 0x774F4632 ('wOF2' in ascii)
                // See: <https://w3c.github.io/woff/woff2/#woff20Header>
                #[cfg(feature = "woff")]
                b"wOF2" => FontFaceSourceFormatKeyword::Woff2,
                // Opentype fonts with CFF data begin with 0x4F54544F ('OTTO' in ascii)
                // See: <https://learn.microsoft.com/en-us/typography/opentype/spec/otff#organization-of-an-opentype-font>
                b"OTTO" => FontFaceSourceFormatKeyword::Opentype,
                // Opentype fonts truetype outlines begin with 0x00010000
                // See: <https://learn.microsoft.com/en-us/typography/opentype/spec/otff#organization-of-an-opentype-font>
                &[0x00, 0x01, 0x00, 0x00] => FontFaceSourceFormatKeyword::Truetype,
                // Truetype fonts begin with 0x74727565 ('true' in ascii)
                // See: <https://developer.apple.com/fonts/TrueType-Reference-Manual/RM06/Chap6.html#ScalerTypeNote>
                b"true" => FontFaceSourceFormatKeyword::Truetype,
                _ => FontFaceSourceFormatKeyword::None,
            }
        }

        // Satisfy rustc's mutability linting with woff feature both enabled/disabled
        #[cfg(feature = "woff")]
        let mut bytes = bytes;

        match self.format {
            #[cfg(feature = "woff")]
            FontFaceSourceFormatKeyword::Woff => {
                #[cfg(feature = "tracing")]
                tracing::info!("Decompressing woff1 font");

                // Use wuff crate to decompress font
                let decompressed = wuff::decompress_woff1(&bytes).ok();

                if let Some(decompressed) = decompressed {
                    bytes = Bytes::from(decompressed);
                } else {
                    #[cfg(feature = "tracing")]
                    tracing::warn!("Failed to decompress woff1 font");
                }
            }
            #[cfg(feature = "woff")]
            FontFaceSourceFormatKeyword::Woff2 => {
                #[cfg(feature = "tracing")]
                tracing::info!("Decompressing woff2 font");

                // Use wuff crate to decompress font
                let decompressed = wuff::decompress_woff2(&bytes).ok();

                if let Some(decompressed) = decompressed {
                    bytes = Bytes::from(decompressed);
                } else {
                    #[cfg(feature = "tracing")]
                    tracing::warn!("Failed to decompress woff2 font");
                }
            }
            FontFaceSourceFormatKeyword::None => {
                // Should this be an error?
                return Ok(Resource::None);
            }
            _ => {}
        }

        Ok(Resource::Font(bytes, std::mem::take(&mut self.overrides)))
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn fetch_font_face(
    tx: Sender<DocumentEvent>,
    doc_id: usize,
    node_id: Option<NodeId>,
    sheet: &Stylesheet,
    network_provider: &Arc<dyn NetProvider>,
    shell_provider: &Arc<dyn ShellProvider>,
    read_guard: &SharedRwLockReadGuard,
    abort_signal: Option<&AbortSignal>,
) {
    sheet
        .contents(read_guard)
        .rules(read_guard)
        .iter()
        .filter_map(|rule| match rule {
            CssRule::FontFace(font_face) => {
                let descriptor = &font_face.read_with(read_guard).descriptors;
                let family = descriptor.font_family.as_ref()?;
                let src = descriptor.src.as_ref()?;
                // Capture the @font-face descriptors so parley can register
                // the font under the CSS-declared family name (and weight /
                // style) rather than whatever metadata the TTF reports.
                let overrides = FontFaceOverrides {
                    family_name: Some(family.name.to_string()),
                    weight: descriptor
                        .font_weight
                        .as_ref()
                        .and_then(|range| range.0.compute().map(|w| w.value())),
                    style: descriptor.font_style.as_ref().map(stylo_to_fontique_style),
                };
                Some((src, overrides))
            }
            _ => None,
        })
        .for_each(|(source_list, overrides)| {
            // Find the first font source in the source list that specifies a font of a type
            // that we support.
            let preferred_source = source_list
                .0
                .iter()
                .filter_map(|source| match source {
                    Source::Url(url_source) => Some(url_source),
                    // TODO: support local fonts in @font-face
                    Source::Local(_) => None,
                })
                .find_map(|url_source| {
                    let mut format = match &url_source.format_hint {
                        Some(FontFaceSourceFormat::Keyword(fmt)) => *fmt,
                        Some(FontFaceSourceFormat::String(str)) => match str.as_str() {
                            "woff2" => FontFaceSourceFormatKeyword::Woff2,
                            "ttf" => FontFaceSourceFormatKeyword::Truetype,
                            "otf" => FontFaceSourceFormatKeyword::Opentype,
                            _ => FontFaceSourceFormatKeyword::None,
                        },
                        _ => FontFaceSourceFormatKeyword::None,
                    };
                    if format == FontFaceSourceFormatKeyword::None {
                        let (_, end) = url_source.url.as_str().rsplit_once('.')?;
                        format = match end {
                            "woff2" => FontFaceSourceFormatKeyword::Woff2,
                            "woff" => FontFaceSourceFormatKeyword::Woff,
                            "ttf" => FontFaceSourceFormatKeyword::Truetype,
                            "otf" => FontFaceSourceFormatKeyword::Opentype,
                            "svg" => FontFaceSourceFormatKeyword::Svg,
                            "eot" => FontFaceSourceFormatKeyword::EmbeddedOpentype,
                            _ => FontFaceSourceFormatKeyword::None,
                        }
                    }

                    if matches!(
                        format,
                        FontFaceSourceFormatKeyword::Svg
                            | FontFaceSourceFormatKeyword::EmbeddedOpentype
                    ) {
                        #[cfg(feature = "tracing")]
                        tracing::warn!("Skipping unsupported font of type {:?}", format);
                        return None;
                    }

                    #[cfg(not(feature = "woff"))]
                    if matches!(
                        format,
                        FontFaceSourceFormatKeyword::Woff | FontFaceSourceFormatKeyword::Woff2
                    ) {
                        #[cfg(feature = "tracing")]
                        tracing::warn!("Skipping unsupported font of type {:?}", format);
                        return None;
                    }

                    // A relative url with no base url to resolve against
                    // yields None; skip the source instead of panicking
                    let Some(url) = url_source.url.url() else {
                        #[cfg(feature = "tracing")]
                        tracing::warn!("Skipping @font-face source with unresolvable url");
                        return None;
                    };
                    let url = url.as_ref().clone();
                    Some((url, format))
                });

            if let Some((url, format)) = preferred_source {
                let mut request = stamped_request(url, abort_signal);
                // Matches what browsers send, so net providers can tell font fetches apart.
                request.headers.insert(
                    HeaderName::from_static("sec-fetch-dest"),
                    HeaderValue::from_static("font"),
                );
                network_provider.fetch(
                    doc_id,
                    request,
                    ResourceHandler::boxed(
                        tx.clone(),
                        doc_id,
                        node_id,
                        shell_provider.clone(),
                        FontFaceHandler { format, overrides },
                    ),
                );
            }
        })
}

/// Translate stylo's `@font-face` `font-style` descriptor into the fontique
/// `FontStyle` enum used by parley. Stylo encodes Italic and Oblique-with-
/// angle distinctly; CSS's bare `normal` is parsed as `Oblique(0deg, 0deg)`
/// by stylo (see the `FontStyle::parse` impl in stylo's `font_face.rs`), so
/// that pattern is treated as `Normal` here.
fn stylo_to_fontique_style(style: &FontStyleRange) -> parley::fontique::FontStyle {
    use parley::fontique::FontStyle as Fq;
    match style {
        FontStyleRange::Italic => Fq::Italic,
        FontStyleRange::Oblique(min, max) => {
            let angle = min.degrees();
            // Stylo emits `Oblique(0deg, 0deg)` for the literal CSS `normal`
            // keyword. Map that back to `Normal` so parley's font matching
            // doesn't misclassify upright fonts.
            if angle.is_none_or(|a| a == 0.0) && max.degrees().is_none_or(|a| a == 0.0) {
                Fq::Normal
            } else {
                Fq::Oblique(angle)
            }
        }
    }
}

/// Handles HTML fetched for an `<iframe>` element's `src`
pub(crate) struct DocumentSrcHandler;

impl NetHandler for ResourceHandler<DocumentSrcHandler> {
    fn bytes(self: Box<Self>, resolved_url: String, bytes: Bytes) {
        let html = String::from_utf8_lossy(&bytes).into_owned();
        self.respond(resolved_url, Ok(Resource::DocumentSrc(html)));
    }
}

pub struct ImageHandler {
    kind: ImageType,
}
impl ImageHandler {
    pub fn new(kind: ImageType) -> Self {
        Self { kind }
    }
}

impl NetHandler for ResourceHandler<ImageHandler> {
    fn bytes(self: Box<Self>, resolved_url: String, bytes: Bytes) {
        let result = self.data.parse(bytes);
        self.respond(resolved_url, result)
    }
}

impl ImageHandler {
    fn parse(&self, bytes: Bytes) -> Result<Resource, String> {
        let image_err = match decode_oriented(&bytes) {
            Ok((image, orientation)) => {
                let (width, height) = (image.width(), image.height());
                return Ok(Resource::Image(
                    self.kind,
                    width,
                    height,
                    Arc::new(image.into_rgba8().into_raw()),
                    orientation,
                ));
            }
            Err(e) => e.to_string(),
        };

        #[cfg(feature = "svg")]
        let svg_err = {
            use crate::util::parse_svg_image;
            match parse_svg_image(&bytes) {
                Ok(svg) => return Ok(Resource::Svg(self.kind, svg)),
                Err(e) => e.to_string(),
            }
        };
        #[cfg(not(feature = "svg"))]
        let svg_err = "svg feature disabled";

        Err(format!(
            "Could not parse image ({} bytes): image-crate error: {image_err}; svg fallback error: {svg_err}",
            bytes.len()
        ))
    }
}

/// Decode `bytes`, with the pixels turned by their EXIF orientation: css-images-3's
/// initial `image-orientation: from-image`.
fn decode_oriented(
    bytes: &[u8],
) -> image::ImageResult<(image::DynamicImage, image::metadata::Orientation)> {
    use image::ImageDecoder;

    let mut decoder = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .expect("IO errors impossible with Cursor")
        .into_decoder()?;
    // `ImageReader::decode`'s allocation guard, which `into_decoder` leaves out.
    let mut limits = image::Limits::default();
    limits.reserve(decoder.total_bytes())?;
    decoder.set_limits(limits)?;
    let orientation = decoder.orientation()?;
    let mut image = image::DynamicImage::from_decoder(decoder)?;
    image.apply_orientation(orientation);
    Ok((image, orientation))
}

#[cfg(test)]
mod tests {
    use super::*;
    use parley::fontique::FontStyle as Fq;
    use style::values::specified::Angle;

    fn oblique(min_deg: f32, max_deg: f32) -> FontStyleRange {
        FontStyleRange::Oblique(Angle::from_degrees(min_deg), Angle::from_degrees(max_deg))
    }

    #[test]
    fn italic_maps_to_italic() {
        assert_eq!(stylo_to_fontique_style(&FontStyleRange::Italic), Fq::Italic,);
    }

    #[test]
    fn oblique_zero_zero_maps_to_normal() {
        // Stylo parses bare CSS `normal` as `Oblique(0deg, 0deg)`; the
        // helper must round-trip that back to `FontStyle::Normal` so
        // parley's matching doesn't misclassify upright fonts.
        assert_eq!(stylo_to_fontique_style(&oblique(0.0, 0.0)), Fq::Normal);
    }

    #[test]
    fn oblique_single_angle_maps_to_oblique_with_min() {
        assert_eq!(
            stylo_to_fontique_style(&oblique(14.0, 14.0)),
            Fq::Oblique(Some(14.0)),
        );
    }

    #[test]
    fn oblique_range_uses_min_angle() {
        // For a range, fontique's single-angle representation takes the
        // lower bound — confirm `min` (not `max`) is what gets through.
        assert_eq!(
            stylo_to_fontique_style(&oblique(10.0, 20.0)),
            Fq::Oblique(Some(10.0)),
        );
    }

    struct RecordingNetProvider(std::sync::Mutex<Vec<Request>>);
    impl NetProvider for RecordingNetProvider {
        fn fetch(&self, _doc_id: usize, request: Request, _handler: Box<dyn NetHandler>) {
            self.0.lock().unwrap().push(request);
        }
    }

    /// Net providers rely on `Sec-Fetch-Dest: font` to recognise a font fetch
    /// without guessing from the url's extension.
    #[test]
    fn font_face_request_is_marked_as_font_destination() {
        let lock = SharedRwLock::new();
        let sheet = Stylesheet::from_str(
            "@font-face { font-family: Test; src: url(test.ttf); }",
            Url::parse("https://example.com/style.css").unwrap().into(),
            Origin::Author,
            ServoArc::new(lock.wrap(MediaList::empty())),
            lock.clone(),
            None,
            None,
            QuirksMode::NoQuirks,
            AllowImportRules::Yes,
        );
        let recorder = Arc::new(RecordingNetProvider(Default::default()));
        let net_provider: Arc<dyn NetProvider> = recorder.clone();
        let shell_provider: Arc<dyn ShellProvider> =
            Arc::new(blitz_traits::shell::DummyShellProvider);
        let (tx, _rx) = std::sync::mpsc::channel();

        fetch_font_face(
            tx,
            0,
            None,
            &sheet,
            &net_provider,
            &shell_provider,
            &lock.read(),
            None,
        );

        let requests = recorder.0.lock().unwrap();
        assert_eq!(requests.len(), 1, "one @font-face source yields one fetch");
        assert_eq!(
            requests[0].headers.get("sec-fetch-dest"),
            Some(&HeaderValue::from_static("font")),
        );
    }

    /// `http://[` comes from the page, and the host runs under `panic = "abort"`,
    /// so a bad `@import` must cost that one rule and not its neighbours.
    #[test]
    fn an_unparsable_import_url_is_refused_and_a_valid_one_still_fetches() {
        let lock = SharedRwLock::new();
        let recorder = Arc::new(RecordingNetProvider(Default::default()));
        let (tx, _rx) = std::sync::mpsc::channel();
        let loader = StylesheetLoader {
            tx,
            doc_id: 0,
            net_provider: recorder.clone(),
            shell_provider: Arc::new(blitz_traits::shell::DummyShellProvider),
            abort_signal: None,
        };

        let sheet = Stylesheet::from_str(
            r#"@import url("http://["); @import url(good.css); p { color: red }"#,
            Url::parse("https://example.com/style.css").unwrap().into(),
            Origin::Author,
            ServoArc::new(lock.wrap(MediaList::empty())),
            lock.clone(),
            Some(&loader),
            None,
            QuirksMode::NoQuirks,
            AllowImportRules::Yes,
        );

        let requests = recorder.0.lock().unwrap();
        assert_eq!(requests.len(), 1, "only the parsable @import is fetched");
        assert_eq!(requests[0].url.as_str(), "https://example.com/good.css");
        let guard = lock.read();
        assert_eq!(
            sheet.contents(&guard).rules(&guard).len(),
            3,
            "the refused rule keeps its place and the rules after it survive"
        );
    }
}
