//! Draft-dependent constraints as bounded pure data.
//!
//! A control can be perfectly available on the running engine while a staged sibling
//! would make the proposed combination invalid. Expressing that requires shipping
//! *conditions* to consumers — and the one thing we will not ship is producer code.
//! A serialized predicate function would be arbitrary logic executing inside a
//! browser, an ESP32 and an MCP client, with no bound on cost and no way to audit it.
//!
//! So constraints are data: a closed operator vocabulary, a depth and node bound, and
//! three-valued evaluation. Unknown operators are expected, not exceptional — a v1.0
//! consumer will meet v1.4 operators — so the degradation rules matter as much as the
//! vocabulary:
//!
//! * **Visibility fails open.** An unevaluatable constraint never hides a control or
//!   suppresses its observed value. Hiding the control the user needs in order to
//!   escape the state is worse than showing an unconstrained one.
//! * **Permission fails closed.** An unevaluatable constraint is never reported as
//!   satisfied. The consumer marks it as needing producer validation and the producer
//!   re-validates server-side regardless of what the consumer concluded.
//!
//! Those two rules are not in tension: one governs what the user can *see*, the other
//! what the system will *accept*. Matchers and layouts operate on the first and have no
//! access to the second.

use serde::{Deserialize, Serialize};

use super::control::{ControlId, Reason, ReasonCode, ReasonScope};
use super::value::{ControlValue, LaneValue, ValueLane};
use super::vocab::semantic_enum;

/// Maximum nesting depth of a constraint expression.
///
/// Bounded so evaluation cost is predictable on the smallest consumer. A producer
/// needing deeper logic must reproject the change set server-side instead.
pub const MAX_EXPR_DEPTH: usize = 8;

/// Maximum total nodes in a constraint expression.
pub const MAX_EXPR_NODES: usize = 128;

semantic_enum! {
    /// What happens when a constraint's condition holds.
    pub enum ConstraintEffect {
        /// The proposed combination is invalid and must not be applied.
        Invalidates => "invalidates",
        /// The combination is accepted but would have no audible effect.
        MakesInert => "makes_inert",
        /// The combination is valid but only after a restart.
        RequiresRestart => "requires_restart",
        /// One or more choices are not selectable. Hides a *choice*, never the control.
        HidesChoice => "hides_choice",
    }
}

/// Resolves lane values for constraint evaluation.
///
/// Every surface evaluates against the same editor-effective view of a change set, so
/// a web page, an MCP client and a device cannot reach different conclusions about the
/// same draft.
pub trait ValueLookup {
    /// The value of `control`'s `lane`, if the view has one.
    fn lookup(&self, control: &ControlId, lane: &ValueLane) -> Option<&LaneValue>;
}

/// Three-valued evaluation result.
///
/// The third value is the point: "I could not decide" is different from "no", and
/// collapsing them is what lets a stale consumer authorise an invalid combination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Evaluation {
    /// The condition holds.
    True,
    /// The condition does not hold.
    False,
    /// The condition could not be decided by this consumer.
    Unevaluatable {
        /// Why, e.g. an unknown operator or an ungrounded operand.
        reason: ReasonCode,
    },
}

impl Evaluation {
    fn unevaluatable(reason: ReasonCode) -> Self {
        Self::Unevaluatable { reason }
    }

    /// Whether the condition is known not to hold.
    pub fn is_definitely_false(&self) -> bool {
        matches!(self, Self::False)
    }

    /// Whether the condition is known to hold.
    pub fn is_definitely_true(&self) -> bool {
        matches!(self, Self::True)
    }
}

/// What a consumer may show, given an evaluation.
#[derive(Debug, Clone, PartialEq)]
pub enum Visibility {
    /// Show the control and its observed value.
    Show,
    /// Show the control and its observed value, annotated with a reason.
    ShowWithReason(Reason),
}

impl Visibility {
    /// Whether the control and its observed value must still be rendered.
    ///
    /// Always true. There is no evaluation outcome that permits hiding a control.
    pub fn renders_control(&self) -> bool {
        true
    }
}

/// What a consumer may allow, given an evaluation.
#[derive(Debug, Clone, PartialEq)]
pub enum Permission {
    /// The consumer may offer the mutation.
    Allowed,
    /// The consumer must not offer the mutation.
    Denied(Reason),
    /// The consumer could not decide. It may offer the mutation only as a *request*;
    /// the producer decides. It must never report the constraint as satisfied.
    RequiresProducerValidation(Reason),
}

impl Permission {
    /// Whether this permission asserts the constraint is satisfied.
    ///
    /// False for [`Permission::RequiresProducerValidation`]: that is the whole point of
    /// failing closed on permission.
    pub fn asserts_satisfied(&self) -> bool {
        matches!(self, Self::Allowed)
    }
}

/// A bounded pure-data predicate.
///
/// Wire form is `{"op": <operator>, ...operands}`. Unknown operators deserialize to
/// [`Expr::Unrecognized`] with their payload retained, so a v1.0 consumer round-trips
/// a v1.4 constraint unchanged and degrades per the rules above.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// A lane value equals a literal.
    Eq {
        /// Control to read.
        control: ControlId,
        /// Lane to read.
        lane: ValueLane,
        /// Literal to compare against.
        value: ControlValue,
    },
    /// A lane value differs from a literal.
    NotEq {
        /// Control to read.
        control: ControlId,
        /// Lane to read.
        lane: ValueLane,
        /// Literal to compare against.
        value: ControlValue,
    },
    /// A lane value is one of a set of literals.
    OneOf {
        /// Control to read.
        control: ControlId,
        /// Lane to read.
        lane: ValueLane,
        /// Accepted literals.
        values: Vec<ControlValue>,
    },
    /// A numeric lane value falls within an inclusive range.
    InRange {
        /// Control to read.
        control: ControlId,
        /// Lane to read.
        lane: ValueLane,
        /// Inclusive lower bound.
        min: Option<f64>,
        /// Inclusive upper bound.
        max: Option<f64>,
    },
    /// A lane has a grounded value at all.
    IsGrounded {
        /// Control to read.
        control: ControlId,
        /// Lane to read.
        lane: ValueLane,
    },
    /// Every child holds.
    All(Vec<Expr>),
    /// At least one child holds.
    Any(Vec<Expr>),
    /// The child does not hold.
    Not(Box<Expr>),
    /// An operator this build does not recognize.
    Unrecognized {
        /// The unrecognized `op` spelling.
        operator: String,
        /// The raw node, retained so the expression round-trips unchanged.
        raw: serde_json::Value,
    },
}

impl Expr {
    /// Evaluate against a view, using Kleene three-valued logic.
    ///
    /// `All` is `False` if any child is `False`, otherwise `Unevaluatable` if any child
    /// is; `Any` is `True` if any child is `True`, otherwise `Unevaluatable` if any
    /// child is. That ordering matters: a definite answer from one child is enough, and
    /// an undecidable sibling must not upgrade a definite `False` into a maybe.
    pub fn evaluate(&self, view: &dyn ValueLookup) -> Evaluation {
        match self {
            Self::Eq {
                control,
                lane,
                value,
            } => match grounded(view, control, lane) {
                Some(found) => bool_to_eval(found == value),
                None => Evaluation::unevaluatable(ReasonCode::Unobservable),
            },
            Self::NotEq {
                control,
                lane,
                value,
            } => match grounded(view, control, lane) {
                Some(found) => bool_to_eval(found != value),
                None => Evaluation::unevaluatable(ReasonCode::Unobservable),
            },
            Self::OneOf {
                control,
                lane,
                values,
            } => match grounded(view, control, lane) {
                Some(found) => bool_to_eval(values.iter().any(|candidate| candidate == found)),
                None => Evaluation::unevaluatable(ReasonCode::Unobservable),
            },
            Self::InRange {
                control,
                lane,
                min,
                max,
            } => match grounded(view, control, lane).and_then(ControlValue::as_f64) {
                Some(number) => bool_to_eval(
                    min.is_none_or(|bound| number >= bound)
                        && max.is_none_or(|bound| number <= bound),
                ),
                None => Evaluation::unevaluatable(ReasonCode::Unobservable),
            },
            Self::IsGrounded { control, lane } => {
                bool_to_eval(grounded(view, control, lane).is_some())
            }
            Self::All(children) => {
                let mut undecided = None;
                for child in children {
                    match child.evaluate(view) {
                        Evaluation::False => return Evaluation::False,
                        Evaluation::Unevaluatable { reason } => undecided = Some(reason),
                        Evaluation::True => {}
                    }
                }
                match undecided {
                    Some(reason) => Evaluation::unevaluatable(reason),
                    None => Evaluation::True,
                }
            }
            Self::Any(children) => {
                let mut undecided = None;
                for child in children {
                    match child.evaluate(view) {
                        Evaluation::True => return Evaluation::True,
                        Evaluation::Unevaluatable { reason } => undecided = Some(reason),
                        Evaluation::False => {}
                    }
                }
                match undecided {
                    Some(reason) => Evaluation::unevaluatable(reason),
                    None => Evaluation::False,
                }
            }
            Self::Not(child) => match child.evaluate(view) {
                Evaluation::True => Evaluation::False,
                Evaluation::False => Evaluation::True,
                Evaluation::Unevaluatable { reason } => Evaluation::unevaluatable(reason),
            },
            Self::Unrecognized { .. } => Evaluation::unevaluatable(ReasonCode::UnknownConstraint),
        }
    }

    /// Whether this expression and all its descendants use recognized operators.
    pub fn is_fully_recognized(&self) -> bool {
        match self {
            Self::Unrecognized { .. } => false,
            Self::All(children) | Self::Any(children) => {
                children.iter().all(Self::is_fully_recognized)
            }
            Self::Not(child) => child.is_fully_recognized(),
            _ => true,
        }
    }

    /// Node count, including this node.
    pub fn node_count(&self) -> usize {
        1 + match self {
            Self::All(children) | Self::Any(children) => {
                children.iter().map(Self::node_count).sum()
            }
            Self::Not(child) => child.node_count(),
            _ => 0,
        }
    }

    /// Nesting depth, counting this node as depth 1.
    pub fn depth(&self) -> usize {
        1 + match self {
            Self::All(children) | Self::Any(children) => {
                children.iter().map(Self::depth).max().unwrap_or(0)
            }
            Self::Not(child) => child.depth(),
            _ => 0,
        }
    }

    /// Check the expression against the published bounds.
    pub fn validate(&self) -> Result<(), ExprLimit> {
        let depth = self.depth();
        if depth > MAX_EXPR_DEPTH {
            return Err(ExprLimit::DepthExceeded {
                depth,
                max: MAX_EXPR_DEPTH,
            });
        }
        let nodes = self.node_count();
        if nodes > MAX_EXPR_NODES {
            return Err(ExprLimit::NodesExceeded {
                nodes,
                max: MAX_EXPR_NODES,
            });
        }
        Ok(())
    }
}

fn bool_to_eval(value: bool) -> Evaluation {
    if value {
        Evaluation::True
    } else {
        Evaluation::False
    }
}

fn grounded<'a>(
    view: &'a dyn ValueLookup,
    control: &ControlId,
    lane: &ValueLane,
) -> Option<&'a ControlValue> {
    view.lookup(control, lane)
        .and_then(LaneValue::grounded_value)
}

/// A constraint expression that exceeds the published bounds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum ExprLimit {
    /// Nesting is deeper than [`MAX_EXPR_DEPTH`].
    DepthExceeded {
        /// Depth found.
        depth: usize,
        /// Published bound.
        max: usize,
    },
    /// More nodes than [`MAX_EXPR_NODES`].
    NodesExceeded {
        /// Nodes found.
        nodes: usize,
        /// Published bound.
        max: usize,
    },
}

impl Serialize for Expr {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        match self {
            Self::Eq {
                control,
                lane,
                value,
            } => {
                map.serialize_entry("op", "eq")?;
                map.serialize_entry("control", control)?;
                map.serialize_entry("lane", lane)?;
                map.serialize_entry("value", value)?;
            }
            Self::NotEq {
                control,
                lane,
                value,
            } => {
                map.serialize_entry("op", "not_eq")?;
                map.serialize_entry("control", control)?;
                map.serialize_entry("lane", lane)?;
                map.serialize_entry("value", value)?;
            }
            Self::OneOf {
                control,
                lane,
                values,
            } => {
                map.serialize_entry("op", "one_of")?;
                map.serialize_entry("control", control)?;
                map.serialize_entry("lane", lane)?;
                map.serialize_entry("values", values)?;
            }
            Self::InRange {
                control,
                lane,
                min,
                max: upper,
            } => {
                map.serialize_entry("op", "in_range")?;
                map.serialize_entry("control", control)?;
                map.serialize_entry("lane", lane)?;
                if let Some(bound) = min {
                    map.serialize_entry("min", bound)?;
                }
                if let Some(bound) = upper {
                    map.serialize_entry("max", bound)?;
                }
            }
            Self::IsGrounded { control, lane } => {
                map.serialize_entry("op", "is_grounded")?;
                map.serialize_entry("control", control)?;
                map.serialize_entry("lane", lane)?;
            }
            Self::All(children) => {
                map.serialize_entry("op", "all")?;
                map.serialize_entry("of", children)?;
            }
            Self::Any(children) => {
                map.serialize_entry("op", "any")?;
                map.serialize_entry("of", children)?;
            }
            Self::Not(child) => {
                map.serialize_entry("op", "not")?;
                map.serialize_entry("of", child.as_ref())?;
            }
            Self::Unrecognized { raw, .. } => {
                // Re-emit the original node verbatim, including its own "op", so an
                // unknown constraint survives a pass-through projection unchanged.
                if let Some(object) = raw.as_object() {
                    for (key, value) in object {
                        map.serialize_entry(key, value)?;
                    }
                }
            }
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for Expr {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        let raw = serde_json::Value::deserialize(deserializer)?;
        let object = raw
            .as_object()
            .ok_or_else(|| D::Error::custom("constraint expression must be an object"))?;
        let operator = object
            .get("op")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| D::Error::custom("constraint expression requires a string \"op\""))?;

        let field = |name: &str| -> Result<&serde_json::Value, D::Error> {
            object
                .get(name)
                .ok_or_else(|| D::Error::custom(format!("{operator:?} requires {name:?}")))
        };
        let parse = |value: &serde_json::Value, name: &str| -> Result<ControlValue, D::Error> {
            serde_json::from_value(value.clone())
                .map_err(|error| D::Error::custom(format!("{operator:?} {name:?}: {error}")))
        };
        let operand = |name: &str| -> Result<(ControlId, ValueLane), D::Error> {
            let control = field("control")?
                .as_str()
                .map(ControlId::new)
                .ok_or_else(|| D::Error::custom(format!("{name:?} control must be a string")))?;
            let lane = field("lane")?
                .as_str()
                .map(ValueLane::from)
                .ok_or_else(|| D::Error::custom(format!("{name:?} lane must be a string")))?;
            Ok((control, lane))
        };
        let children = |name: &str| -> Result<Vec<Expr>, D::Error> {
            let members = field("of")?
                .as_array()
                .ok_or_else(|| D::Error::custom(format!("{name:?} requires an array \"of\"")))?;
            let mut out = Vec::with_capacity(members.len());
            for member in members {
                out.push(
                    serde_json::from_value(member.clone())
                        .map_err(|error| D::Error::custom(format!("{name:?} child: {error}")))?,
                );
            }
            Ok(out)
        };

        match operator {
            "eq" => {
                let (control, lane) = operand("eq")?;
                Ok(Self::Eq {
                    control,
                    lane,
                    value: parse(field("value")?, "value")?,
                })
            }
            "not_eq" => {
                let (control, lane) = operand("not_eq")?;
                Ok(Self::NotEq {
                    control,
                    lane,
                    value: parse(field("value")?, "value")?,
                })
            }
            "one_of" => {
                let (control, lane) = operand("one_of")?;
                let members = field("values")?
                    .as_array()
                    .ok_or_else(|| D::Error::custom("\"one_of\" requires an array \"values\""))?;
                let mut values = Vec::with_capacity(members.len());
                for member in members {
                    values.push(parse(member, "values")?);
                }
                Ok(Self::OneOf {
                    control,
                    lane,
                    values,
                })
            }
            "in_range" => {
                let (control, lane) = operand("in_range")?;
                Ok(Self::InRange {
                    control,
                    lane,
                    min: object.get("min").and_then(serde_json::Value::as_f64),
                    max: object.get("max").and_then(serde_json::Value::as_f64),
                })
            }
            "is_grounded" => {
                let (control, lane) = operand("is_grounded")?;
                Ok(Self::IsGrounded { control, lane })
            }
            "all" => Ok(Self::All(children("all")?)),
            "any" => Ok(Self::Any(children("any")?)),
            "not" => {
                let child = serde_json::from_value(field("of")?.clone())
                    .map_err(|error| D::Error::custom(format!("\"not\" child: {error}")))?;
                Ok(Self::Not(Box::new(child)))
            }
            other => Ok(Self::Unrecognized {
                operator: other.to_string(),
                raw: raw.clone(),
            }),
        }
    }
}

/// A named constraint over one or more controls.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Constraint {
    /// Stable semantic constraint id, so a consumer can suppress a duplicate warning
    /// without string-matching a message.
    pub id: String,
    /// Controls this constraint speaks about.
    pub applies_to: Vec<ControlId>,
    /// The condition. The effect applies when this evaluates to
    /// [`Evaluation::True`].
    pub when: Expr,
    /// What holds when the condition holds.
    pub effect: ConstraintEffect,
    /// Machine-readable reason, with a catalog key for display text.
    pub reason: Reason,
}

impl Constraint {
    /// What a consumer may show, given an evaluation of [`Constraint::when`].
    ///
    /// Never hides. An undecidable constraint yields an annotation, not a disappearance.
    pub fn visibility(&self, evaluation: &Evaluation) -> Visibility {
        match evaluation {
            Evaluation::False => Visibility::Show,
            Evaluation::True => Visibility::ShowWithReason(self.reason.clone()),
            Evaluation::Unevaluatable { reason } => Visibility::ShowWithReason(Reason {
                code: reason.clone(),
                scope: ReasonScope::Draft,
                display_text_key: self.reason.display_text_key.clone(),
                detail: None,
            }),
        }
    }

    /// What a consumer may allow, given an evaluation of [`Constraint::when`].
    ///
    /// Fails closed. An undecidable constraint is never reported as satisfied, and the
    /// producer re-validates server-side regardless.
    pub fn permission(&self, evaluation: &Evaluation) -> Permission {
        match evaluation {
            Evaluation::False => Permission::Allowed,
            Evaluation::True => match self.effect {
                // Only `Invalidates` denies. An inert or restart-requiring combination
                // is a legitimate thing for a user to choose deliberately.
                ConstraintEffect::Invalidates => Permission::Denied(self.reason.clone()),
                _ => Permission::Allowed,
            },
            Evaluation::Unevaluatable { reason } => {
                Permission::RequiresProducerValidation(Reason {
                    code: reason.clone(),
                    scope: ReasonScope::Draft,
                    display_text_key: self.reason.display_text_key.clone(),
                    detail: None,
                })
            }
        }
    }
}
