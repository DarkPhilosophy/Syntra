//! How a strip's pixels reach the screen.
//!
//! A [`Surface`] is one window along one edge. A renderer draws a [`Paint`]
//! into it. The choice of renderer is a setting: the user may force the GPU or
//! the CPU, or leave it to the plugin to use the GPU and fall back to the CPU
//! where it cannot. That fallback is the point of this module, so it lives
//! here, away from any window, where it can be tested.

use std::fmt;

use crate::{paint::Paint, state::Renderer};

/// Why a renderer could not be used.
#[derive(Debug)]
pub struct RenderError(pub String);

impl fmt::Display for RenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for RenderError {}

/// Draws strips of glow.
pub trait Draw {
    /// A short name for logs and for the plugin manager.
    fn name(&self) -> &'static str;

    /// Draws `paint` into a strip `width` by `height` pixels and presents it.
    fn draw(&mut self, paint: &Paint, width: u32, height: u32) -> Result<(), RenderError>;

    /// Presents a fully transparent strip, so a window that cannot be hidden
    /// at once does not keep showing the last frame.
    fn clear(&mut self, width: u32, height: u32) -> Result<(), RenderError>;
}

/// One way of making a renderer, tried in order.
///
/// `C` is whatever the renderer needs in order to be made (for windows, the
/// event loop). It is passed in when the candidate is tried rather than
/// captured, so a candidate holds no long-lived reference to it.
pub struct Candidate<C: ?Sized, T> {
    /// Which setting value this is.
    pub kind: Renderer,
    /// Makes the renderer, or says why it cannot.
    pub make: Box<dyn FnMut(&C) -> Result<T, RenderError>>,
}

/// What came of choosing a renderer.
#[derive(Debug, PartialEq, Eq)]
pub struct Choice {
    /// Which candidate was used.
    pub used: Renderer,
    /// Candidates that were tried first and failed, with their reasons, so a
    /// fallback is something a user can find out about rather than a mystery.
    pub skipped: Vec<(Renderer, String)>,
}

/// Picks the renderer the setting asks for.
///
/// `Auto` tries each candidate in the order given (the GPU before the CPU) and
/// takes the first that works. A specific choice tries only that one and does
/// not quietly substitute another: a user who asked for the GPU and got the
/// CPU would not know their setting had been ignored.
pub fn choose<C: ?Sized, T>(
    wanted: Renderer,
    context: &C,
    candidates: &mut [Candidate<C, T>],
) -> Result<(T, Choice), RenderError> {
    let mut skipped = Vec::new();
    for candidate in candidates.iter_mut() {
        if wanted != Renderer::Auto && wanted != candidate.kind {
            continue;
        }
        match (candidate.make)(context) {
            Ok(renderer) => {
                return Ok((
                    renderer,
                    Choice {
                        used: candidate.kind,
                        skipped,
                    },
                ));
            }
            Err(error) => skipped.push((candidate.kind, error.0)),
        }
    }
    let reasons = skipped
        .iter()
        .map(|(kind, why)| format!("{kind:?}: {why}"))
        .collect::<Vec<_>>()
        .join("; ");
    Err(RenderError(if reasons.is_empty() {
        format!("no renderer is available for {wanted:?}")
    } else {
        format!("no renderer could be started ({reasons})")
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn working(kind: Renderer) -> Candidate<(), Renderer> {
        Candidate {
            kind,
            make: Box::new(move |_| Ok(kind)),
        }
    }

    fn broken(kind: Renderer, why: &'static str) -> Candidate<(), Renderer> {
        Candidate {
            kind,
            make: Box::new(move |_| Err(RenderError(why.into()))),
        }
    }

    /// Automatic takes the GPU when it works.
    #[test]
    fn auto_prefers_the_first_candidate_that_works() {
        let mut list = [working(Renderer::OpenGl), working(Renderer::Software)];
        let (renderer, choice) = choose(Renderer::Auto, &(), &mut list).unwrap();
        assert_eq!(renderer, Renderer::OpenGl);
        assert!(choice.skipped.is_empty());
    }

    /// Automatic falls back to the CPU and records why, instead of failing or
    /// hiding that the GPU was not used.
    #[test]
    fn auto_falls_back_and_says_why() {
        let mut list = [
            broken(Renderer::OpenGl, "no EGL configuration"),
            working(Renderer::Software),
        ];
        let (renderer, choice) = choose(Renderer::Auto, &(), &mut list).unwrap();
        assert_eq!(renderer, Renderer::Software);
        assert_eq!(
            choice.skipped,
            vec![(Renderer::OpenGl, "no EGL configuration".to_owned())]
        );
    }

    /// A user who chose the GPU is not silently given the CPU.
    #[test]
    fn a_specific_choice_is_never_substituted() {
        let mut list = [
            broken(Renderer::OpenGl, "no EGL configuration"),
            working(Renderer::Software),
        ];
        let error = choose(Renderer::OpenGl, &(), &mut list).err().unwrap();
        assert!(error.0.contains("no EGL configuration"), "{error}");
    }

    /// And a specific choice is honoured even when another would also work.
    #[test]
    fn a_specific_choice_skips_the_others() {
        let mut list = [working(Renderer::OpenGl), working(Renderer::Software)];
        let (renderer, choice) = choose(Renderer::Software, &(), &mut list).unwrap();
        assert_eq!(renderer, Renderer::Software);
        assert!(choice.skipped.is_empty(), "OpenGl was not even attempted");
    }

    #[test]
    fn nothing_available_is_an_error_not_a_panic() {
        let mut list: [Candidate<(), Renderer>; 0] = [];
        assert!(choose(Renderer::Auto, &(), &mut list).is_err());
        let mut all_broken = [
            broken(Renderer::OpenGl, "a"),
            broken(Renderer::Software, "b"),
        ];
        let error = choose(Renderer::Auto, &(), &mut all_broken).err().unwrap();
        assert!(error.0.contains("OpenGl: a") && error.0.contains("Software: b"));
    }
}
