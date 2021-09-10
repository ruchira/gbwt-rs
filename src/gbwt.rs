//! GBWT: A run-length encoded FM-index storing paths as sequences of node identifiers.
//!
//! The GBWT was originally described in:
//!
//! > Sirén, Garrison, Novak, Paten, Durbin: **Haplotype-aware graph indexes**.  
//! > Bioinformatics, 2020. DOI: [10.1093/bioinformatics/btz575](https://doi.org/10.1093/bioinformatics/btz575)
//!
//! At the moment, this implementation only supports GBWT indexes built with other tools.
//! See also the original [C++ implementation](https://github.com/jltsiren/gbwt).

use crate::{ENDMARKER, SOURCE_KEY, SOURCE_VALUE};
use crate::bwt::BWT;
use crate::headers::{Header, GBWTPayload, MetadataPayload};
use crate::support::{Dictionary, StringIter, Tags};
use crate::support;

use simple_sds::serialize::{Serialize, Serializable};
use simple_sds::serialize;

use std::io::{Error, ErrorKind};
use std::iter::FusedIterator;
use std::ops::Range;
use std::{io, slice};

#[cfg(test)]
mod tests;

//-----------------------------------------------------------------------------

/// The GBWT index storing a collection of paths space-efficiently.
///
/// The GBWT stores integer sequences.
/// Each integer is assumed to be a node identifier, and each sequence is interpreted as a path in a graph.
/// If the index is not bidirectional, GBWT node and sequence identifiers correspond directly to node and path identifiers in the original graph.
///
/// In a bidirectional index, each node (path) in the original graph becomes two nodes (sequences) in the GBWT: one for the forward orientation and one for the reverse orientation.
/// A reverse path visits the other orientation of each node on the path in reverse order.
/// The following functions can be used for mapping between the identifiers used by the GBWT and the graph:
///
/// * [`support::encode_node`], [`support::flip_node`], [`support::node_id`], and [`support::node_is_reverse`] for node identifiers.
/// * [`support::encode_path`], [`support::flip_path`], [`support::path_id`], and [`support::path_is_reverse`] for sequence / path identifiers.
///
/// # Examples
///
/// ```
/// use gbwt::{GBWT, SearchState};
/// use gbwt::support;
/// use simple_sds::serialize;
///
/// let filename = support::get_test_data("example.gbwt");
/// let index: GBWT = serialize::load_from(&filename).unwrap();
///
/// // Statistics.
/// assert_eq!(index.len(), 68);
/// assert_eq!(index.sequences(), 12);
/// assert_eq!(index.alphabet_size(), 52);
/// assert!(index.is_bidirectional());
///
/// // Manually find the second-to-last node of path 2 in forward orientation.
/// let mut pos = index.start(support::encode_path(2, false));
/// let mut last = None;
/// while pos.is_some() {
///     last = pos;
///     pos = index.forward(pos.unwrap());
/// }
/// let (node, _) = index.backward(last.unwrap()).unwrap();
/// assert_eq!(node, support::encode_node(15, false));
///
/// // Search for subpath (12, forward), (14, forward), (15, forward).
/// let state = index.find(support::encode_node(12, false)).unwrap();
/// let state = index.extend(&state, support::encode_node(14, false)).unwrap();
/// let state = index.extend(&state, support::encode_node(15, false)).unwrap();
/// assert_eq!(state.node, support::encode_node(15, false));
/// assert_eq!(state.len(), 2);
///
/// // Bidirectional search for the same subpath.
/// let state = index.bd_find(support::encode_node(14, false)).unwrap();
/// let state = index.extend_backward(&state, support::encode_node(12, false)).unwrap();
/// let state = index.extend_forward(&state, support::encode_node(15, false)).unwrap();
/// assert_eq!(state.forward.node, support::encode_node(15, false));
/// assert_eq!(state.reverse.node, support::encode_node(12, true));
/// assert_eq!(state.len(), 2);
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GBWT {
    header: Header<GBWTPayload>,
    tags: Tags,
    bwt: BWT,
    endmarker: Vec<(usize, usize)>,
}

/// Index statistics.
impl GBWT {
    /// Returns the total length of the sequences in the index.
    #[inline]
    pub fn len(&self) -> usize {
        self.header.payload().size
    }

    /// Returns `true` if the index is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the number of sequences in the index.
    #[inline]
    pub fn sequences(&self) -> usize {
        self.header.payload().sequences
    }

    /// Returns the size of the alphabet.
    #[inline]
    pub fn alphabet_size(&self) -> usize {
        self.header.payload().alphabet_size
    }

    /// Returns the alphabet offset for the effective alphabet.
    #[inline]
    pub fn alphabet_offset(&self) -> usize {
        self.header.payload().offset
    }

    /// Returns the size of the effective alphabet.
    #[inline]
    pub fn effective_size(&self) -> usize {
        self.alphabet_size() - self.alphabet_offset()
    }

    /// Returns the smallest node identifier in the effective alphabet.
    #[inline]
    pub fn first_node(&self) -> usize {
        self.alphabet_offset() + 1
    }

    // Converts node id to record id.
    #[inline]
    fn node_to_record(&self, i: usize) -> usize {
        i - self.alphabet_offset()
    }

    /// Returns `true` if node identifier `id` is in the effective alphabet.
    #[inline]
    pub fn has_node(&self, id: usize) -> bool {
        id > self.alphabet_offset() && id < self.alphabet_size()
    }

    /// Returns `true` if the GBWT index is bidirectional.
    #[inline]
    pub fn is_bidirectional(&self) -> bool {
        self.header.is_set(GBWTPayload::FLAG_BIDIRECTIONAL)
    }
}

//-----------------------------------------------------------------------------

/// Sequence navigation.
impl GBWT {
    /// Returns the first position in sequence `id`, or [`None`] if no such sequence exists.
    ///
    /// The return value is a pair (node identifier, offset in node).
    pub fn start(&self, id: usize) -> Option<(usize, usize)> {
        if id < self.endmarker.len() {
            Some(self.endmarker[id])
        } else {
            None
        }
    }

    /// Follows the sequence forward and returns the next position, or [`None`] if no such position exists.
    ///
    /// The argument and the return value are pairs (node identifier, offset in node).
    pub fn forward(&self, pos: (usize, usize)) -> Option<(usize, usize)> {
        // This also catches the endmarker.
        if pos.0 < self.first_node() {
            return None;
        }
        if let Some(record) = self.bwt.record(self.node_to_record(pos.0)) {
            return record.lf(pos.1);
        }
        None
    }

    /// Follows the sequence backward and returns the previous position, or [`None`] if no such position exists.
    ///
    /// The argument and the return value are pairs (node identifier, offset in node).
    ///
    /// # Panics
    ///
    /// Panics if the index is not bidirectional.
    pub fn backward(&self, pos: (usize, usize)) -> Option<(usize, usize)> {
        assert!(self.is_bidirectional(), "Following sequences backward requires a bidirectional GBWT");
        // This also catches the endmarker.
        if pos.0 <= self.first_node() {
            return None;
        }
        let reverse_id = self.node_to_record(support::flip_node(pos.0));
        if let Some(record) = self.bwt.record(reverse_id) {
            if let Some(predecessor) = record.predecessor_at(pos.1) {
                if let Some(pred_record) = self.bwt.record(self.node_to_record(predecessor)) {
                    if let Some(offset) = pred_record.offset_to(pos) {
                        return Some((predecessor, offset));
                    }
                }
            }
        }
        None
    }

    /// Returns an iterator over sequence `id`.
    ///
    /// The iterator will be empty if no such sequence exists.
    pub fn sequence(&self, id: usize) -> SequenceIter {
        SequenceIter {
            parent: self,
            next: self.start(id),
        }
    }
}

//-----------------------------------------------------------------------------

/// Subpath search.
impl GBWT {
    /// Returns a search state for all occurrences of the given node, or [`None`] if no such node exists.
    pub fn find(&self, node: usize) -> Option<SearchState> {
        // This also catches the endmarker.
        if node < self.first_node() {
            return None;
        }
        if let Some(record) = self.bwt.record(self.node_to_record(node)) {
            return Some(SearchState {
                node: node,
                range: 0..record.len(),
            });
        }
        None
    }

    /// Extends the search by the given node forward and returns the new search state, or [`None`] if no such extensions exist.
    ///
    /// Assume that the current search state corresponds to a set of substring occurrences ending with the same node.
    /// This method takes all of those substrings that continue with the given node, extends them with that node, and returns the new search state.
    ///
    /// # Arguments
    ///
    /// * `state`: A search state corresponding to a set of substring occurrences.
    /// * `node`: Node to extend the substrings with.
    pub fn extend(&self, state: &SearchState, node: usize) -> Option<SearchState> {
        // This also catches the endmarker.
        if node < self.first_node() {
            return None;
        }
        if let Some(record) = self.bwt.record(self.node_to_record(state.node)) {
            if let Some(range) = record.follow(&state.range, node) {
                return Some(SearchState {
                    node: node,
                    range: range,
                })
            }
        }
        None
    }

    /// Returns a bidirectional search state for all occurrences of the given node, or [`None`] if no such node exists.
    ///
    /// # Panics
    ///
    /// Will panic if the index is not bidirectional.
    pub fn bd_find(&self, node: usize) -> Option<BidirectionalState> {
        assert!(self.is_bidirectional(), "Bidirectional search requires a bidirectional GBWT");
        if let Some(state) = self.find(node) {
            let reverse = SearchState {
                node: support::flip_node(state.node),
                range: state.range.clone(),
            };
            return Some(BidirectionalState {
                forward: state,
                reverse: reverse,
            });
        }
        None
    }

    /// Extends the search by the given node forward and returns the new search state, or [`None`] if no such extensions exist.
    ///
    /// Assume that the current search state corresponds to a set of substring occurrences ending with the same node.
    /// This method takes all of those substrings that continue with the given node, extends them with that node, and returns the new search state.
    ///
    /// # Arguments
    ///
    /// * `state`: A bidirectional search state corresponding to a set of substring occurrences.
    /// * `node`: Node to extend the substrings with.
    ///
    /// # Panics
    ///
    /// Will panic if the index is not bidirectional.
    pub fn extend_forward(&self, state: &BidirectionalState, node: usize) -> Option<BidirectionalState> {
        assert!(self.is_bidirectional(), "Bidirectional search requires a bidirectional GBWT");
        // This also catches the endmarker.
        if node < self.first_node() {
            return None;
        }
        if let Some(record) = self.bwt.record(self.node_to_record(state.forward.node)) {
            if let Some((range, offset)) = record.bd_follow(&state.forward.range, node) {
                let forward = SearchState {
                    node: node,
                    range: range,
                };
                let pos = state.reverse.range.start + offset;
                let reverse = SearchState {
                    node: state.reverse.node,
                    range: pos..pos + forward.len(),
                };
                return Some(BidirectionalState {
                    forward: forward,
                    reverse: reverse,
                });
            }
        }
        None
    }

    /// Extends the search by the given node backward and returns the new search state, or [`None`] if no such extensions exist.
    ///
    /// Assume that the current search state corresponds to a set of substring occurrences starting with the same node.
    /// This method takes all of those substrings that are preceded by the given node, extends them with that node, and returns the new search state.
    ///
    /// # Arguments
    ///
    /// * `state`: A bidirectional search state corresponding to a set of substring occurrences.
    /// * `node`: Node to extend the substrings with.
    ///
    /// # Panics
    ///
    /// Will panic if the index is not bidirectional.
    pub fn extend_backward(&self, state: &BidirectionalState, node: usize) -> Option<BidirectionalState> {
        if let Some(result) = self.extend_forward(&state.flip(), support::flip_node(node)) {
            return Some(result.flip());
        }
        None
    }
}

//-----------------------------------------------------------------------------

impl Serialize for GBWT {
    fn serialize_header<T: io::Write>(&self, writer: &mut T) -> io::Result<()> {
        self.header.serialize(writer)
    }

    fn serialize_body<T: io::Write>(&self, writer: &mut T) -> io::Result<()> {
        self.tags.serialize(writer)?;
        self.bwt.serialize(writer)?;
        serialize::absent_option(writer)?; // Document array samples.
        serialize::absent_option(writer)?; // Metadata. TODO: Support
        Ok(())
    }

    fn load<T: io::Read>(reader: &mut T) -> io::Result<Self> {
        let mut header = Header::<GBWTPayload>::load(reader)?;
        if let Err(msg) = header.validate() {
            return Err(Error::new(ErrorKind::InvalidData, msg));
        }
        header.unset(GBWTPayload::FLAG_METADATA); // TODO: We do not handle metadata at the moment.

        let mut tags = Tags::load(reader)?;
        tags.insert(SOURCE_KEY, SOURCE_VALUE);

        let bwt = BWT::load(reader)?;

        // Decompress the endmarker, as the record can be poorly compressible.
        let endmarker = if bwt.is_empty() { Vec::new() } else { bwt.record(ENDMARKER).unwrap().decompress() };

        serialize::skip_option(reader)?; // Document array samples.
        serialize::skip_option(reader)?; // Metadata. TODO: Support

        Ok(GBWT {
            header: header,
            tags: tags,
            bwt: bwt,
            endmarker: endmarker,
        })
    }

    fn size_in_elements(&self) -> usize {
        self.header.size_in_elements() + self.tags.size_in_elements() + self.bwt.size_in_elements() + 2 * serialize::absent_option_size()
    }
}

//-----------------------------------------------------------------------------

/// A state of unidirectional search in [`GBWT`].
///
/// The state consists of the last matched GBWT node identifier and an offset range in that node.
/// This information is equivalent to a BWT range in a normal FM-index.
///
/// Note that because `SearchState` contains a [`Range`], which does not implement [`Copy`], states must often be passed by reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchState {
    /// GBWT node identifier for the last matched node.
    pub node: usize,
    /// Offset range in the node.
    pub range: Range<usize>,
}

impl SearchState {
    /// Returns the number of matching substring occurrences (the length of the offset range).
    #[inline]
    pub fn len(&self) -> usize {
        self.range.len()
    }

    /// Returns `true` if there are no matching substring occurrences.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.range.is_empty()
    }
}

/// A state of bidirectional search in a bidirectional [`GBWT`].
///
/// The state consists of forward and reverse search states.
/// It usually corresponds to all occurrences of a substring `pattern`.
/// The forward state is then the search state for `pattern`, while the reverse state is for the reverse pattern obtained with [`support::reverse_path`].
///
/// Note that because `BidirectionalState` contains a [`Range`], which does not implement [`Copy`], states must often be passed by reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BidirectionalState {
    /// GBWT search state for the forward pattern.
    pub forward: SearchState,
    /// GBWT search state for the reverse pattern.
    pub reverse: SearchState,
}

impl BidirectionalState {
    /// Returns the number of matching substring occurrences (the length of the offset range).
    #[inline]
    pub fn len(&self) -> usize {
        self.forward.len()
    }

    /// Returns `true` if there are no matching substring occurrences.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.forward.is_empty()
    }

    /// Returns a new search state with the forward and reverse states flipped.
    pub fn flip(&self) -> BidirectionalState {
        BidirectionalState {
            forward: self.reverse.clone(),
            reverse: self.forward.clone(),
        }
    }
}

//-----------------------------------------------------------------------------

/// An iterator over a sequence in [`GBWT`].
///
/// The type of `Item` is [`usize`].
///
/// # Examples
///
/// ```
/// use gbwt::GBWT;
/// use gbwt::support;
/// use simple_sds::serialize;
///
/// let filename = support::get_test_data("example.gbwt");
/// let index: GBWT = serialize::load_from(&filename).unwrap();
///
/// // Extract path 3 in reverse orientation.
/// let path: Vec<usize> = index.sequence(support::encode_path(3, true)).collect();
/// assert_eq!(path, vec![35, 33, 29, 27, 23]);
/// ```
#[derive(Clone, Debug)]
pub struct SequenceIter<'a> {
    parent: &'a GBWT,
    // The next position.
    next: Option<(usize, usize)>,
}

impl<'a> Iterator for SequenceIter<'a> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(pos) = self.next {
            self.next = self.parent.forward(pos);
            return Some(pos.0);
        } else {
            return None;
        }
    }
}

impl<'a> FusedIterator for SequenceIter<'a> {}

//-----------------------------------------------------------------------------

// FIXME document, example
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Metadata {
    header: Header<MetadataPayload>,
    path_names: Vec<PathName>,
    sample_names: Dictionary,
    contig_names: Dictionary,
}

impl Metadata {
    /// Returns `true` if the metadata contains path names.
    pub fn has_path_names(&self) -> bool {
        self.header.is_set(MetadataPayload::FLAG_PATH_NAMES)
    }

    /// Returns `true` if the metadata contains sample names.
    pub fn has_sample_names(&self) -> bool {
        self.header.is_set(MetadataPayload::FLAG_SAMPLE_NAMES)
    }

    /// Returns `true` if the metadata contains contig names.
    pub fn has_contig_names(&self) -> bool {
        self.header.is_set(MetadataPayload::FLAG_CONTIG_NAMES)
    }

    /// Returns the number path names in the metadata.
    ///
    /// If there are path names, each name corresponds to a path in the original graph.
    /// In a bidirectional GBWT, there are twice as many sequences as paths.
    pub fn paths(&self) -> usize {
        self.path_names.len()
    }

    /// Returns the number samples.
    pub fn samples(&self) -> usize {
        self.header.payload().sample_count
    }

    /// Returns the number of haplotypes.
    ///
    /// This generally corresponds to the number of full-length paths in a graph component.
    pub fn haplotypes(&self) -> usize {
        self.header.payload().haplotype_count
    }

    /// Returns the number contigs.
    ///
    /// A contig usually corresponds to a graph component.
    pub fn contigs(&self) -> usize {
        self.header.payload().contig_count
    }

    /// Returns the name of the given path, or [`None`] if there is no such name.
    pub fn path(&self, i: usize) -> Option<PathName> {
        if i < self.paths() {
            Some(self.path_names[i])
        } else {
            None
        }
    }

    /// Returns the name of the given sample, or [`None`] if there is no such name.
    ///
    /// Also returns [`None`] if the name exists but is not valid UTF-8.
    pub fn sample(&self, i: usize) -> Option<&str> {
        if self.has_sample_names() && i < self.samples() {
            self.sample_names.str(i).ok()
        } else {
            None
        }
    }

    /// Returns the name of the given contig, or [`None`] if there is no such name.
    ///
    /// Also returns [`None`] if the name exists but is not valid UTF-8.
    pub fn contig(&self, i: usize) -> Option<&str> {
        if self.has_contig_names() && i < self.contigs() {
            self.contig_names.str(i).ok()
        } else {
            None
        }
    }

    /// Returns an iterator over path names.
    pub fn path_iter(&self) -> slice::Iter<PathName> {
        self.path_names.iter()
    }

    /// Returns an iterator over sample names.
    pub fn sample_iter(&self) -> StringIter {
        self.sample_names.as_ref().iter()
    }

    /// Returns an iterator over contig names.
    pub fn contig_iter(&self) -> StringIter {
        self.contig_names.as_ref().iter()
    }
}

impl Serialize for Metadata {
    fn serialize_header<T: io::Write>(&self, writer: &mut T) -> io::Result<()> {
        self.header.serialize(writer)
    }

    fn serialize_body<T: io::Write>(&self, writer: &mut T) -> io::Result<()> {
        self.path_names.serialize(writer)?;
        self.sample_names.serialize(writer)?;
        self.contig_names.serialize(writer)?;
        Ok(())
    }

    fn load<T: io::Read>(reader: &mut T) -> io::Result<Self> {
        let header = Header::<MetadataPayload>::load(reader)?;
        if let Err(msg) = header.validate() {
            return Err(Error::new(ErrorKind::InvalidData, msg));
        }

        let path_names = Vec::<PathName>::load(reader)?;
        if header.is_set(MetadataPayload::FLAG_PATH_NAMES) == path_names.is_empty() {
            return Err(Error::new(ErrorKind::InvalidData, "Metadata: Path name flag does not match the presence of path names"));
        }

        let sample_names = Dictionary::load(reader)?;
        if header.is_set(MetadataPayload::FLAG_SAMPLE_NAMES) {
            if header.payload().sample_count != sample_names.len() {
                return Err(Error::new(ErrorKind::InvalidData, "Metadata: Sample count does not match the number of sample names"));
            }
        } else if !sample_names.is_empty() {
            return Err(Error::new(ErrorKind::InvalidData, "Metadata: Sample names are present without the sample name flag"));
        }

        let contig_names = Dictionary::load(reader)?;
        if header.is_set(MetadataPayload::FLAG_CONTIG_NAMES) {
            if header.payload().contig_count != contig_names.len() {
                return Err(Error::new(ErrorKind::InvalidData, "Metadata: Contig count does not match the number of contig names"));
            }
        } else if !contig_names.is_empty() {
            return Err(Error::new(ErrorKind::InvalidData, "Metadata: Contig names are present without the contig name flag"));
        }

        Ok(Metadata {
            header: header,
            path_names: path_names,
            sample_names: sample_names,
            contig_names: contig_names,
        })
    }

    fn size_in_elements(&self) -> usize {
        self.header.size_in_elements() + self.path_names.size_in_elements() + self.sample_names.size_in_elements() + self.contig_names.size_in_elements()
    }
}

//-----------------------------------------------------------------------------

// FIXME document
/// A structured path name.
///
/// Each name has four components: sample, contig, phase / haplotype, and fragment / count.
/// FIXME semantics and constraints for components
#[repr(C)]
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct PathName {
    /// Sample identifier.
    pub sample: u32,

    /// Contig identifier.
    pub contig: u32,

    /// Phase / haplotype identifier.
    pub phase: u32,

    /// Fragment identifier / running count.
    pub fragment: u32,
}

impl PathName {
    /// Returns a new path name with all components set to 0.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a path name with the given values in each field.
    pub fn from_fields(sample: usize, contig: usize, phase: usize, fragment: usize) -> Self {
        PathName {
            sample: sample as u32,
            contig: contig as u32,
            phase: phase as u32,
            fragment: fragment as u32,
        }
    }
}

impl Serializable for PathName {}

//-----------------------------------------------------------------------------
