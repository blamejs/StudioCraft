//! Face tracking on the device, from camera pictures to head pose and expression weights.
//!
//! The models are the Face Landmarker bundle (`face_landmarker.task`, Apache-2.0), which the app
//! downloads or the user installs; the crate holds no model data. [`Bundle::from_task`] reads it:
//!
//! - the face detector (`face_detector.tflite`, 128 × 128 input, 896 anchors),
//! - the face mesh (`face_landmarks_detector.tflite`, 256 × 256 input, 478 points with depth, a
//!   face presence score and a tongue score),
//! - the canonical face and its fit weights (`geometry_pipeline_metadata_landmarks.binarypb`).
//!
//! The models run on [`tflite`], a reader and interpreter for the float operators they use, with
//! every buffer sized when the model loads. [`Tracker`] follows one face from frame to frame: it
//! runs the detector only while no face is followed, then one mesh run per frame, and turns the
//! mesh into a [`HeadPose`] (a weighted rigid fit to the canonical face, [`pose`]) and
//! [`Expressions`] (measured from the mesh in the head's own frame, [`expressions`]), smoothed with
//! One Euro filters ([`filter`]). Once its buffers are warm, following a face does not allocate.
//!
//! [`models`] holds the catalog of model files with their pinned URL, size and SHA-256, the
//! checks of a model in the models folder, and the install of a file the user already has.
//!
//! Every tracker runs its models on a [`Pool`]. Trackers that share one pool share its threads,
//! so the pool's size caps the threads all of them use together.
//!
//! The crate depends on no StudioCraft crate.

pub mod bundle;
pub mod expressions;
pub mod filter;
pub mod frame;
pub mod landmarker;
pub mod models;
mod nn;
pub mod pool;
pub mod pose;
pub mod tflite;
pub mod tracker;

pub use bundle::{Bundle, MAX_BUNDLE_BYTES};
pub use expressions::{Blendshape, Expressions};
pub use filter::{OneEuro, OneEuroParams, RotationFilter, Smoothing};
pub use frame::{Frame, Roi};
pub use landmarker::{Detection, Landmarker, MESH_POINTS, Mesh};
pub use pool::Pool;
pub use pose::HeadPose;
pub use tracker::{FaceChoice, TrackFrame, Tracker, TrackerOptions};

/// What can go wrong reading a model bundle or tracking a face.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum Error {
    /// The model bundle could not be read, or is not the face landmarker bundle.
    #[error("the face tracking model could not be read: {0}")]
    Bundle(String),
    /// A model inside the bundle could not be loaded or run.
    #[error("the face tracking model could not run: {0}")]
    Model(String),
    /// A camera picture's size and buffer disagree.
    #[error("the camera picture is not usable: {0}")]
    Frame(String),
    /// The tracker's threads could not start, or a thread count is out of range.
    #[error("{0}")]
    Threads(String),
    /// A model file could not be checked or put in the models folder.
    #[error("the face tracking model could not be installed: {0}")]
    Install(String),
    /// A model install was stopped.
    #[error("the face tracking model install was canceled")]
    Canceled,
}

/// A result with this crate's [`Error`].
pub type Result<T> = std::result::Result<T, Error>;
