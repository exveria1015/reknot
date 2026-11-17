"""Validate and summarize reload_latency's raw, open-loop measurements (stdlib only)."""
import argparse
import json
import math
import statistics
from collections import defaultdict
from pathlib import Path


def stats(values):
    values = sorted(values)
    if not values:
        return None
    return {
        "n": len(values),
        "mean": statistics.mean(values),
        "median": values[len(values) // 2],
        "p95": values[math.ceil(len(values) * .95) - 1],
        "p99": values[math.ceil(len(values) * .99) - 1],
        "min": values[0],
        "max": values[-1],
    }


def analyze(directory):
    manifest = json.loads((directory / "manifest.json").read_text(encoding="utf-8"))
    assert manifest["complete"], "incomplete benchmark run"
    grouped = defaultdict(lambda: defaultdict(list))
    samples = []
    for run in manifest["runs"]:
        data = json.loads((directory / run["file"]).read_text(encoding="utf-8"))
        case, settings = data["case"], data["settings"]
        assert len(data["samples"]) == settings["samples"] + settings["warmups"]
        for sample in data["samples"]:
            requests, polls = sample["requests"], sample["owner_polls"]
            assert requests and polls
            scheduled = sorted(r["scheduled_ms"] for r in requests)
            # Every intended arrival was submitted and completed, including
            # those whose scheduled time fell inside an owner-thread stall.
            for index, stamp in enumerate(scheduled):
                assert abs(stamp - index * settings["interval_us"] / 1000) < .0001
            assert all(r["completed_ms"] >= r["submitted_ms"] >= r["scheduled_ms"] for r in requests)
            assert all(r["generation"] is not None for r in requests)
            first_save = sample["saves"][0]["start_ms"]
            final_save = sample["saves"][-1]["start_ms"]
            activation = sample["activation_ms"]
            first_new = sample["first_new_response_ms"]
            assert first_new >= activation >= final_save
            events = sample["events"]
            transitions = [e for e in events if e["kind"] in ("restarting", "reloaded")]
            candidate_start = transitions[0]["start_ms"]
            quiet_requests = [r for r in requests if r["completed_ms"] < first_save]
            background_requests = [r for r in requests if r["scheduled_ms"] >= first_save and r["completed_ms"] < candidate_start]
            # Include requests already in flight at the start of the transition;
            # do not discard the delayed completions caused by the transition.
            transition_requests = [r for r in requests if r["completed_ms"] >= candidate_start and r["scheduled_ms"] <= activation + 100]
            update_requests = [r for r in requests if r["completed_ms"] >= first_save and r["scheduled_ms"] <= activation + 200]
            quiet_polls = [p for p in polls if p["start_ms"] + p["elapsed_ms"] < first_save]
            update_polls = [p for p in polls if p["start_ms"] >= first_save]
            release_polls = [p for p in polls if p["released"]]
            assert quiet_requests and background_requests and transition_requests and quiet_polls
            assert bool(sample["pause"]) == case["restart"]
            assert bool(release_polls) == case["managed"]
            if case["managed"]:
                assert sum(p["released"] for p in polls) == 1, "expected exactly one retired image"
                assert sample["old_image_released_ms"] >= activation
            if case["managed"] and not case["restart"]:
                assert sample["stop_worker_ms"] >= activation + 50
                assert sample["old_image_released_ms"] >= sample["worker_done_ms"] >= sample["stop_worker_ms"]
            if sample["warmup"]:
                continue
            group = grouped[run["case"]]
            row = {"case": run["case"], "round": run["round"], "index": sample["index"],
                   "save_to_response_ms": first_new - final_save,
                   "first_save_to_response_ms": first_new - first_save,
                   "save_to_activation_ms": activation - final_save,
                   "activation_to_response_ms": first_new - activation,
                   "max_owner_update_ms": max(p["elapsed_ms"] for p in update_polls),
                   "superseded": sum(e["kind"] == "superseded" for e in events),
                   "builds_started": sum(e["kind"] == "building" for e in events),
                   "responses": len(requests), "background_responses": len(background_requests)}
            for name, subset in [("quiet", quiet_requests), ("background", background_requests),
                                 ("transition", transition_requests), ("update", update_requests)]:
                values = [r["completed_ms"] - r["scheduled_ms"] for r in subset]
                group[f"{name}_request_ms"].extend(values)
                row[f"{name}_request_ms"] = stats(values)
            group["quiet_owner_ms"].extend(p["elapsed_ms"] for p in quiet_polls)
            group["update_owner_ms"].extend(p["elapsed_ms"] for p in update_polls)
            group["request_submission_lag_ms"].extend(r["submitted_ms"] - r["scheduled_ms"] for r in requests)
            group["bridge_driver_ms"].extend(p[1] for p in sample["driver_polls"])
            for kind in ("restarting", "reloaded"):
                group[f"{kind}_owner_ms"].extend(e["end_ms"] - e["start_ms"] for e in events if e["kind"] == kind)
            group["post_activation_owner_ms"].extend(p["elapsed_ms"] for p in polls if activation < p["start_ms"] < activation + 100)
            if sample["pause"]:
                for name in ("observed_ms", "lower_ms", "upper_ms"):
                    row[f"pause_{name}"] = sample["pause"][name]
            if case["managed"]:
                row["activation_to_reclaim_ms"] = sample["old_image_released_ms"] - activation
                row["reclaim_owner_ms"] = max(p["elapsed_ms"] for p in release_polls)
                row["collect_ms"] = sum(p["collect_ms"] for p in release_polls)
                if not case["restart"]:
                    row["stop_to_worker_done_ms"] = sample["worker_done_ms"] - sample["stop_worker_ms"]
            for key, value in sample["build_metrics"].items() if sample["build_metrics"] else []:
                row[f"jit_{key}"] = value
            for key, value in row.items():
                if key not in ("round", "index") and isinstance(value, (int, float)):
                    group[key].append(value)
            samples.append(row)
    summary = {name: {key: stats(values) for key, values in group.items() if values}
               for name, group in grouped.items()}
    for name, group in summary.items():
        runs = sum(r["case"] == name for r in manifest["runs"])
        assert group["save_to_response_ms"]["n"] == runs * manifest["settings"]["samples"]
    result = {"manifest": manifest, "summary": summary, "samples": samples,
              "definitions": {
                  "median": "upper middle sample, consistent with development.rs",
                  "percentiles": "nearest rank; request percentiles pool correlated requests, not independent reload trials",
                  "request_ms": "scheduled arrival to completion, including late submission and bridge admission queue",
                  "quiet": "completed before first save within the same sample",
                  "background": "scheduled after first save and completed before candidate owner work",
                  "transition": "completion after candidate owner work begins and scheduled by activation + 100 ms",
                  "update": "completion after first save and scheduled by activation + 200 ms",
                  "pause": "observed endpoints after update calls; lower/upper bounds bracket actual admission closure",
                  "owner": "whole DevModule::update, including cleanup/workers/collection but excluding the separately polled bridge",
              }}
    (directory / "analysis.json").write_text(json.dumps(result, indent=2), encoding="utf-8")
    print(f"{manifest['os']}: {len(samples)} measured reloads, raw arrival/reclamation invariants passed")
    print("case | save median | quiet req p99 | background req p99 | transition req p99 | owner max | pause observed median")
    for name, group in summary.items():
        def value(key, field):
            return f"{group[key][field]:.3f}" if key in group else "-"
        print(" | ".join([name, value("save_to_response_ms", "median"), value("quiet_request_ms", "p99"),
                          value("background_request_ms", "p99"), value("transition_request_ms", "p99"),
                          value("update_owner_ms", "max"), value("pause_observed_ms", "median")]))
    return result


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directories", type=Path, nargs="+")
    args = parser.parse_args()
    for directory in args.directories:
        analyze(directory)
