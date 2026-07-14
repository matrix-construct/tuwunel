//! Budget-driven text segmentation. The size budget is injected as a `fits`
//! predicate, so the caller owns measurement; markdown mode keeps code fences
//! self-contained across segment boundaries.

use std::iter::once;

/// Byte offset past a line, paired with the fence info open after it.
type Line<'a> = (usize, Option<&'a str>);

struct Chunker<'a, F> {
	rest: &'a str,
	markdown: bool,
	fits: F,

	/// Info string of a code fence left open by the previous segment, reopened
	/// at the head of the next.
	fence: Option<&'a str>,

	emitted: bool,
}

/// Where the next segment ends.
enum Cut<'a> {
	/// The whole remainder fits: consume it as the final segment.
	All,

	/// Cut before this byte offset, carrying this fence state to the next
	/// segment (a closing fence is injected when it is open).
	At {
		offset: usize,
		fence: Option<&'a str>,
	},

	/// No whole line fits; the first line must be split at a character
	/// boundary.
	Line,
}

/// Splits `text` into segments that each satisfy `fits`, packing whole lines
/// greedily. In markdown mode a code fence spanning a segment boundary is
/// closed at the tail of one segment and reopened with the same info string at
/// the head of the next, so each segment renders on its own; a single line
/// larger than the budget is split at a character boundary. Empty input yields
/// one empty segment. The iterator is lazy: a bounded `take` stops it before
/// the whole (possibly large) input is scanned.
pub fn chunk<F>(text: &str, markdown: bool, fits: F) -> impl Iterator<Item = String>
where
	F: Fn(&str) -> bool,
{
	Chunker {
		rest: text,
		markdown,
		fits,
		fence: None,
		emitted: false,
	}
}

impl<F> Iterator for Chunker<'_, F>
where
	F: Fn(&str) -> bool,
{
	type Item = String;

	fn next(&mut self) -> Option<Self::Item> {
		if self.rest.is_empty() {
			return (!self.emitted).then(|| {
				self.emitted = true;
				String::new()
			});
		}

		self.emitted = true;

		Some(self.segment())
	}
}

impl<'a, F> Chunker<'a, F>
where
	F: Fn(&str) -> bool,
{
	fn segment(&mut self) -> String {
		let head = self.fence.map_or_else(String::new, reopen);

		match self.cut(&head) {
			| Cut::All => {
				let segment = assemble(&head, self.rest, false);
				self.rest = "";
				self.fence = None;

				segment
			},
			| Cut::At { offset, fence } => {
				let (body, rest) = self.rest.split_at(offset);
				let segment = assemble(&head, body, fence.is_some());
				self.rest = rest;
				self.fence = fence;

				segment
			},
			| Cut::Line => self.split_line(&head),
		}
	}

	/// Finds the greatest whole-line prefix of `self.rest` that fits, by an
	/// exponential then binary search over line count. Detects when the whole
	/// remainder fits, and reports when not even one line does.
	fn cut(&self, head: &str) -> Cut<'a> {
		let rest = self.rest;
		let mut lines: Vec<Line<'a>> = Vec::new();

		let split_fits = |(end, fence): Line<'a>| {
			let (body, _) = rest.split_at(end);

			(self.fits)(&assemble(head, body, fence.is_some()))
		};

		let mut lo = None; // greatest line index known to fit as a proper split
		let mut hi; // least line index known not to fit
		let mut probe: usize = 0;

		loop {
			discover(rest, self.fence, self.markdown, &mut lines, probe.saturating_add(1));

			if lines
				.get(probe)
				.is_none_or(|&(end, _)| end == rest.len())
			{
				if (self.fits)(&assemble(head, rest, false)) {
					return Cut::All;
				}

				hi = lines.len().saturating_sub(1);
				break;
			}

			if split_fits(lines[probe]) {
				lo = Some(probe);
				probe = probe.saturating_mul(2).saturating_add(1);
			} else {
				hi = probe;
				break;
			}
		}

		let Some(mut lo) = lo else {
			return Cut::Line;
		};

		while hi.abs_diff(lo) > 1 {
			let mid = lo.midpoint(hi);

			if split_fits(lines[mid]) {
				lo = mid;
			} else {
				hi = mid;
			}
		}

		let (offset, fence) = lines[lo];

		Cut::At { offset, fence }
	}

	/// Splits the first line at the greatest character boundary that fits,
	/// advancing at least one character so progress is guaranteed. The fence
	/// state is unchanged: a partial line toggles nothing.
	fn split_line(&mut self, head: &str) -> String {
		let rest = self.rest;
		let line = rest.split_inclusive('\n').next().unwrap_or(rest);

		let bounds: Vec<usize> = line
			.char_indices()
			.skip(1)
			.map(|(i, _)| i)
			.chain(once(line.len()))
			.collect();

		let prefix_fits =
			|&bound: &usize| (self.fits)(&assemble(head, line.split_at(bound).0, false));

		let split = bounds
			.partition_point(prefix_fits)
			.checked_sub(1)
			.map_or(bounds[0], |i| bounds[i]);

		let (line, rest) = rest.split_at(split);
		self.rest = rest;

		assemble(head, line, false)
	}
}

/// Extends `lines` to cover at least `upto` lines of `rest`, or until `rest`
/// is exhausted. Each entry pairs the byte offset past a line with the fence
/// info open after it (markdown mode only).
fn discover<'a>(
	rest: &'a str,
	start_fence: Option<&'a str>,
	markdown: bool,
	lines: &mut Vec<Line<'a>>,
	upto: usize,
) {
	let (mut end, mut fence) = lines.last().copied().unwrap_or((0, start_fence));
	let (_, tail) = rest.split_at(end);

	for line in tail
		.split_inclusive('\n')
		.take(upto.saturating_sub(lines.len()))
	{
		end = end.saturating_add(line.len());

		if markdown {
			toggle_fence(&mut fence, line);
		}

		lines.push((end, fence));
	}
}

/// Toggles `fence` when `line` is a code-fence delimiter: an outside delimiter
/// opens a fence carrying its info string, an inside delimiter closes it.
fn toggle_fence<'a>(fence: &mut Option<&'a str>, line: &'a str) {
	let Some(info) = fence_line_info(line) else {
		return;
	};

	*fence = fence.is_none().then_some(info);
}

/// The info string of a triple-backtick fence line, or `None` when the line is
/// not a fence delimiter.
fn fence_line_info(line: &str) -> Option<&str> {
	line.trim_start()
		.strip_prefix("```")
		.map(|rest| rest.trim_start_matches('`').trim())
}

fn reopen(info: &str) -> String { format!("```{info}\n") }

/// Joins the reopened head, body, and an optional closing fence into a segment.
fn assemble(head: &str, body: &str, close: bool) -> String {
	match close {
		| true => format!("{head}{body}```\n"),
		| false => format!("{head}{body}"),
	}
}
