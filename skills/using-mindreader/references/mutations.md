# Mutation reference

Read this file before revise, withdraw, place, judge, or unify, and before using contradiction or review-queue features on a write.

## Contents

- Write facts
- Revise or withdraw facts
- Place memberships
- Review and unify identities
- Judge retrievals

## Write facts

Write proactively when discussion, investigation, decisions, implementation, debugging, review, or handoff establishes supported knowledge that another agent or session should reuse. The user need not ask. Send 1–20 facts in one atomic `mindreader:write` call with call-level `scope`. Reasserting an exact fact merges memberships or is a no-op; another object preserves existing values.

Subjects and entity objects are `{"kind":"node","iri":"..."}` or `{"kind":"node","name":"..."}`. Literals are `{"kind":"literal","value":"...","datatype":"xsd:string"}`; `datatype` defaults to `xsd:string`. A predicate accepts its name or IRI.

```json
{
  "scope": ["project:graph-memory"],
  "facts": [
    {
      "s": {"kind": "node", "name": "Alice"},
      "p": "worksOn",
      "o": {"kind": "node", "name": "Mindreader"},
      "spike": "Knowledge"
    }
  ]
}
```

SPIKE progresses `Signal -> Pattern -> Insight -> Knowledge`: use Signal for raw evidence, Pattern for recurrence, Insight for interpretation, and Knowledge only for a fact worth relying on. Retrieval priority is the reverse. Do not auto-promote. Name-only subjects stay `mindreader:element/<slug>`; `spike` is an extra label and ranking, not a new IRI kind. Reuse established Property/Class vocabulary; make one bounded catalog recall only when vocabulary is uncertain.

## Revise or withdraw facts

When current work establishes that stored knowledge is wrong, obsolete, or no longer true, maintain it without waiting for a user correction request. Use an exact fact target retained from an earlier result or obtained through one targeted recall.

- `mindreader:revise` replaces only the object. Its `scope` selects memberships removed from the old fact; the replacement receives the full requested scope, while unrelated current values and memberships remain. `scope: []` selects a global fact. A global fact visible through a named scope cannot be revised there.
- `mindreader:withdraw` softly removes selected memberships and preserves history. Prefer an exact fact `target`. Subject form withdraws every mutable ordinary current outgoing fact for the subject in the selected memberships, optionally restricted by `p`; use it only when that broad slice is intentional. Supply exactly one of `target` or `subject`. A named scope cannot withdraw a global fact; use `scope: []` only when a global withdrawal is intended.
- If revision or withdrawal removes a fact's final selected named membership, the old fact becomes historical; it does not become global.

```json
{
  "scope": ["project:graph-memory"],
  "target": {"kind": "fact", "iri": "mindreader:relationship/<returned-uuid>"},
  "new": {"kind": "node", "name": "Memory Platform"},
  "reason": "Project was renamed"
}
```

## Place memberships

Use `mindreader:place` only when membership itself should change. `scope` controls target visibility; each of 1–20 unique edits supplies `add` and/or `remove`.

Final fact memberships must be visible through both endpoints: a global endpoint permits any fact membership; otherwise each endpoint must contain every membership of the fact. Include literal objects in the same batch as `{kind:"node", iri}`. Batch related node and fact edits so Mindreader validates their combined final state atomically.

Removing a target's final named membership stores an empty membership list and makes it global. To move a target between named layers without exposing it globally, add the destination and remove the source in the same edit.

```json
{
  "scope": ["project:graph-memory"],
  "edits": [
    {
      "target": {"kind": "fact", "iri": "mindreader:relationship/<returned-uuid>"},
      "add": ["team:shared"],
      "remove": ["project:graph-memory"]
    }
  ]
}
```

## Review and unify identities

`review.unify` contains fuzzy same-kind candidates, not proof. Use `sourceName` / `targetName` as display context, ignore irrelevant suggestions, and call `mindreader:unify` only when evidence independently establishes that two nodes are the same identity and which IRI/name should survive.

Paste `review.unify[].source` and `.target`, or `handles.unify[]`, as node handles. Unify has no `scope`, is database-wide and permanent, reconciles all memberships and history, and has no undo. The `target` survives; the source node is removed and no alias is created. If identity or direction remains uncertain, skip it.

```json
{
  "source": {"kind": "node", "iri": "mindreader:element/alice-duplicate"},
  "target": {"kind": "node", "iri": "mindreader:element/alice"}
}
```

`review.alternatives` is advisory and does not mean another set-valued fact is wrong.

## Judge retrievals

After a recalled node or current fact clearly helped or misled, optionally send 1–20 unique targets in one atomic `mindreader:judge` call. `strengthen` changes shared weight by exactly `+1`; `weaken` changes it by `-1`. Weight is shared across memberships, never decays, and orders results only within the same Spike category; it is retrieval feedback, not confidence or recency. Recall never changes it automatically.

```json
{
  "scope": ["project:graph-memory"],
  "ratings": [
    {
      "target": {"kind": "fact", "iri": "mindreader:relationship/<returned-uuid>"},
      "mode": "strengthen"
    }
  ]
}
```

Correct or withdraw false memory for truth; weaken it only when retrieval quality also merits that signal. Never substitute judgment for correction.
