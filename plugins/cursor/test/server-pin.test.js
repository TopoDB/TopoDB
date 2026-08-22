import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { SERVER_VERSION } from "../core/server-args.js";

const pkg = JSON.parse(readFileSync(new URL("../package.json", import.meta.url), "utf8"));
test("the server the launcher installs is the server the e2e test validates", () => {
  // SERVER_VERSION (launch.js's pin, via server-args.js) and
  // devDependencies["@topodb/topodb-mcp"] (what the e2e test actually
  // exercises) are two hand-synced copies of the same fact. If they drift,
  // the e2e suite stays green while validating a server version no user ever
  // launches — exactly the drift that shipped @topodb/pi with a stale 0.0.3
  // for a week, unnoticed.
  assert.equal(SERVER_VERSION, pkg.devDependencies["@topodb/topodb-mcp"]);
});

test("the plugin ships the server version this repo actually builds", () => {
  // The test above pins the plugin's two copies of the version to EACH OTHER,
  // which keeps them honest but lets them drift together, away from the repo.
  // That happened: the workspace went to topodb-mcp 0.0.6 (engine format v4)
  // while the plugin still installed 0.0.5 (format v3), and every test stayed
  // green — because nothing compared the pin to the crate.
  //
  // This is not cosmetic. `topodb` 0.0.7's migration to format v4 is ONE-WAY:
  // once a memory.redb is opened by the newer server it cannot be reopened by
  // an older one. Shipping a stale server is how a user's memory gets stranded.
  //
  // So: a crate bump must turn this suite RED, forcing a deliberate plugin
  // bump, rather than silently shipping yesterday's engine.
  //
  // Exception: the pin may stay on the last published npm version while the
  // crate has moved on — `npm install` in CI 404s on an unpublished pin.
  // Drop the 0.0.20 lag once @topodb/topodb-mcp@0.1.0 is on the registry.
  const cargo = readFileSync(
    new URL("../../../crates/topodb-mcp/Cargo.toml", import.meta.url),
    "utf8",
  );
  const crateVersion = cargo.match(/^version\s*=\s*"([^"]+)"/m)?.[1];
  assert.ok(crateVersion, "could not read version from crates/topodb-mcp/Cargo.toml");
  const pinMatchesCrate = SERVER_VERSION === crateVersion;
  const pinLagsUnpublishedCrate = SERVER_VERSION === "0.0.20" && crateVersion === "0.1.0";
  assert.ok(
    pinMatchesCrate || pinLagsUnpublishedCrate,
    `plugin pins topodb-mcp ${SERVER_VERSION} but this repo builds ${crateVersion}. ` +
      `Bump SERVER_VERSION and the devDependency once ${crateVersion} is published to npm.`,
  );
});
