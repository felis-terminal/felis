import { createHash, randomUUID } from "node:crypto";
import fs from "node:fs";
import path from "node:path";

const DAY_MS = 24 * 60 * 60 * 1000;
const MAX_AGE_DAYS = 7;
// Above the timeout-minutes of every job that uses cargo-env.
const MAX_CLAIM_HOURS = 6;

const env = process.env;
const log = (line) => console.log(`cargo-env: ${line}`);
const gb = (bytes) => `${(bytes / 1024 ** 3).toFixed(1)} GB`;

function readJson(file) {
  try {
    return JSON.parse(fs.readFileSync(file, "utf8"));
  } catch {
    return null;
  }
}

function required(name) {
  const value = env[name];
  if (!value) throw new Error(`${name} is not set`);
  return value;
}

function start() {
  const root = required("CARGO_ENV_ROOT");
  const target = path.join(root, "target");
  const jobs = path.join(root, "jobs");
  const stampFile = path.join(root, "stamp.json");
  const self = {
    job: `${env.GITHUB_WORKFLOW} / ${env.GITHUB_JOB} (run ${env.GITHUB_RUN_ID})`,
    runner: required("CARGO_ENV_RUNNER"),
    temp: required("RUNNER_TEMP"),
    started: Date.now(),
  };
  log(`runner ${self.runner}`);

  // A job's RUNNER_TEMP lives exactly as long as the job, whichever way the
  // job ends, but a runner that dies mid-job never removes it, and a file
  // held open by a leftover process can keep it from being removed: the
  // runner identity and the claim's age catch those.
  const alive = (claim) =>
    claim !== null &&
    claim.runner === self.runner &&
    self.started - claim.started < MAX_CLAIM_HOURS * 60 * 60 * 1000 &&
    fs.existsSync(claim.temp);

  fs.mkdirSync(jobs, { recursive: true });
  const live = [];
  const dead = [];
  for (const name of fs.readdirSync(jobs)) {
    const file = path.join(jobs, name);
    const claim = readJson(file);
    (alive(claim) ? live : dead).push({ file, job: claim?.job ?? name });
  }

  const sha256 = (data) => createHash("sha256").update(data).digest("hex");
  const inputs = {
    toolchain: sha256(required("CARGO_ENV_TOOLCHAIN").trim()),
    "Cargo.lock": sha256(fs.readFileSync("Cargo.lock")),
    "profile overrides": Object.keys(env)
      .filter((name) => name.startsWith("CARGO_PROFILE_"))
      .sort()
      .map((name) => `${name}=${env[name]}`)
      .join(" "),
  };

  const reasons = [];
  if (dead.length > 0) {
    reasons.push(`interrupted before releasing it: ${dead.map((d) => d.job).join("; ")}`);
  }
  const stamp = readJson(stampFile);
  if (stamp === null) {
    reasons.push("it carries no stamp");
  } else {
    const days = (self.started - stamp.created) / DAY_MS;
    log(`built from scratch ${days.toFixed(1)} days ago`);
    if (days > MAX_AGE_DAYS) reasons.push(`older than ${MAX_AGE_DAYS} days`);
    const changed = Object.keys(inputs).filter((name) => stamp.inputs?.[name] !== inputs[name]);
    if (changed.length > 0) {
      const what = `${changed.join(", ")} changed since then`;
      // A pull request that changes Cargo.lock or the toolchain would
      // otherwise rebuild on every alternation with the branches that do not.
      if (env.GITHUB_EVENT_NAME === "pull_request") {
        log(`${what}; a pull request keeps the directory`);
      } else {
        reasons.push(what);
      }
    }
  }
  const minFree = Number(env.CARGO_ENV_MIN_FREE_BYTES ?? 0);
  if (minFree > 0) {
    const { bavail, bsize } = fs.statfsSync(root);
    const free = bavail * bsize;
    log(`${gb(free)} free on the disk`);
    if (free < minFree) reasons.push(`the disk is down to ${gb(free)} free`);
  }

  if (reasons.length === 0) {
    log("reusing the build directory");
  } else if (live.length > 0) {
    log(`not rebuilding (${reasons.join("; ")}) while running jobs use it: ${live.map((l) => l.job).join("; ")}`);
  } else {
    log(`rebuilding from scratch: ${reasons.join("; ")}`);
    fs.rmSync(target, { recursive: true, force: true, maxRetries: 5 });
    for (const { file } of dead) fs.rmSync(file, { force: true });
    fs.writeFileSync(stampFile, JSON.stringify({ inputs, created: self.started }));
  }

  fs.mkdirSync(target, { recursive: true });
  const marker = path.join(jobs, `${randomUUID()}.json`);
  fs.writeFileSync(marker, JSON.stringify(self));
  fs.appendFileSync(
    required("GITHUB_ENV"),
    [`CARGO_TARGET_DIR=${target}`, `CARGO_ENV_MARKER=${marker}`, "CARGO_TERM_COLOR=always", ""].join("\n"),
  );
}

function done() {
  const marker = env.CARGO_ENV_MARKER;
  if (!marker) {
    log("this job never claimed the build directory");
    return;
  }
  const claim = readJson(marker);
  fs.rmSync(marker, { force: true });
  log("released the build directory");

  let total = 0;
  let pdb = 0;
  let pdbCount = 0;
  let pdbWritten = 0;
  const walk = (dir) => {
    let entries;
    try {
      entries = fs.readdirSync(dir, { withFileTypes: true });
    } catch {
      return;
    }
    for (const entry of entries) {
      const file = path.join(dir, entry.name);
      if (entry.isDirectory()) {
        walk(file);
        continue;
      }
      let stat;
      try {
        stat = fs.statSync(file);
      } catch {
        continue;
      }
      total += stat.size;
      if (entry.name.endsWith(".pdb")) {
        pdb += stat.size;
        pdbCount += 1;
        if (claim !== null && stat.mtimeMs >= claim.started) pdbWritten += stat.size;
      }
    }
  };
  walk(required("CARGO_TARGET_DIR"));
  log(`the build directory holds ${gb(total)}`);
  if (pdbCount > 0) log(`${gb(pdb)} of it in ${pdbCount} PDB files, ${gb(pdbWritten)} written by this job`);
}

const phases = { start, done };
const phase = phases[process.argv[2]];
if (!phase) throw new Error(`usage: guard.mjs ${Object.keys(phases).join("|")}`);
phase();
