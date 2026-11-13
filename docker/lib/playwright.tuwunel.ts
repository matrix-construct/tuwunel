import {
    AbstractStartedContainer,
    GenericContainer,
    type StartedNetwork,
    type StartedTestContainer,
    Wait,
} from "testcontainers";
import {type APIRequestContext, type TestInfo} from "@playwright/test";
import crypto from "node:crypto";

import {
    type HomeserverContainer,
    type StartedHomeserverContainer,
    type StartedMatrixAuthenticationServiceContainer,
} from "@element-hq/element-web-playwright-common/lib/testcontainers/index.js";
import {ClientServerApi, type Credentials} from "@element-hq/element-web-playwright-common/lib/utils/api.js";

// Matches the `registration_shared_secret` baked into the testee image at
// /playwright/playwright.toml. The endpoint accepts any value, but the testee
// rejects mismatches with a 403, so this constant is load-bearing.
const REGISTRATION_SHARED_SECRET = "playwright-shared-secret";

// The minimum config surface tests are allowed to flip at runtime. The full
// config lives in tuwunel.toml; the in-image baked file is the source of
// truth. withConfigField/withConfig accept any key, but only the keys listed
// here are forwarded as -O flags at startup.
const DEFAULT_CONFIG = {
    log: "warn,tuwunel=info",
};
export type TuwunelConfig = typeof DEFAULT_CONFIG;

/**
 * A Playwright homeserver container backed by Tuwunel.
 *
 * Warning: this is an unstable API/interface and may change without notice.
 */
export class TuwunelContainer extends GenericContainer implements HomeserverContainer<TuwunelConfig> {
    protected config: TuwunelConfig;
    private cmdOverrides: string[] = [];

    /**
     * Creates a new TuwunelContainer.
     * @param image The image tag to start from. Defaults to the locally-built testee image.
     */
    public constructor(image = "tuwunel-playwright-testee:latest") {
        super(image);

        this.config = {...DEFAULT_CONFIG};

        // The testee joins the per-shard network (see start()) and publishes no
        // host port, so the HTTP wait strategy has no mapped port to probe; gate
        // readiness on tuwunel's "Listening on" startup line instead.
        this.withWaitStrategy(Wait.forLogMessage(/Listening on/)).withStartupTimeout(30_000);
    }

    /**
     * Overrides a single config field for this container.
     * @param key The config key to override.
     * @param value The value to set.
     * @returns This container, for chaining.
     */
    public withConfigField<Key extends keyof TuwunelConfig>(key: Key, value: TuwunelConfig[Key]): this {
        (this.config as Record<string, unknown>)[key as string] = value;
        if (isTuwunelConfigField(String(key))) {
            this.cmdOverrides.push(toOverride(String(key), value));
        }
        return this;
    }

    /**
     * Overrides multiple config fields for this container.
     * @param config A partial config to merge over the defaults.
     * @returns This container, for chaining.
     */
    public withConfig(config: Partial<TuwunelConfig>): this {
        this.config = {...this.config, ...config};
        for (const [k, v] of Object.entries(config)) {
            if (isTuwunelConfigField(k)) {
                this.cmdOverrides.push(toOverride(k, v));
            }
        }
        return this;
    }

    /**
     * No-op: the tuwunel test harness does not yet ship an SMTP server.
     * Specs that require email verification must be on the skip-list.
     * @returns This container, for chaining.
     */
    public withSmtpServer(): this {
        return this; // XXX: SMTP support not implemented
    }

    /**
     * No-op: tuwunel does not integrate with MAS in the test harness.
     * Specs that require MAS must be on the skip-list.
     * @returns This container, for chaining.
     */
    public withMatrixAuthenticationService(_mas?: StartedMatrixAuthenticationServiceContainer): this {
        return this; // XXX: MAS integration not implemented
    }

    /**
     * No-op: the testee joins the per-shard network in start(), not the per-test
     * network the homeserver fixture creates here. The siblings on that fixture
     * network (MAS, SMTP, postgres) are all no-ops for tuwunel, so its
     * withNetwork/withNetworkAliases attach is dead weight; absorbing it keeps a
     * leftover alias from conflicting with the per-shard attach.
     * @returns This container, for chaining.
     */
    public override withNetwork(_network: StartedNetwork): this {
        return this;
    }

    /**
     * No-op: see withNetwork.
     * @returns This container, for chaining.
     */
    public override withNetworkAliases(..._aliases: string[]): this {
        return this;
    }

    public override async start(): Promise<StartedTuwunelContainer> {
        // The tester and every testee share a per-shard user-defined bridge
        // network (docker/playwright.sh, PLAYWRIGHT_NETWORK). Each testee binds
        // the container-internal port 8008 in its own netns and the tester
        // reaches it by container-name DNS, so no host port is published: the
        // shards on the shared daemon cannot collide on a port, and the tester
        // is off host networking, so sibling veth churn no longer aborts
        // in-flight Chromium requests (net::ERR_NETWORK_CHANGED, chromium
        // #974711). The name survives homeserver.restart(), so baseUrl does too.
        const network = process.env.PLAYWRIGHT_NETWORK;
        if (!network) {
            throw new Error("PLAYWRIGHT_NETWORK is unset; see docker/playwright.sh");
        }

        // element-web's specs expect homeserver.restart() to wipe the server, as
        // Synapse's in-memory sqlite does: login-consent registers the same user
        // again after each restart. Docker empties a tmpfs whenever the container
        // stops, so a database_path (docker/Dockerfile.playwright) mounted there
        // comes back empty after restart() as well.
        this.withTmpFs({"/database": "rw"});

        // The baked image entrypoint is shell-form, which ignores CMD; override it
        // with an exec-form entrypoint so the per-container -O flags reach tuwunel.
        // address 0.0.0.0 so siblings on the shared network can reach it.
        this.withNetworkMode(network)
            .withEntrypoint(["tuwunel"])
            .withCommand([
                toOverride("server_name", "localhost"),
                toOverride("port", 8008),
                toOverride("address", "0.0.0.0"),
                ...this.cmdOverrides,
            ]);

        const container = await super.start();
        const host = container.getName().replace(/^\//, "");
        const baseUrl = `http://${host}:8008`;
        return new StartedTuwunelContainer(container, baseUrl, REGISTRATION_SHARED_SECRET);
    }
}

/**
 * Formats a tuwunel `-O<key>=<value>` config override. `-O` parses each value
 * as TOML, so string values are quoted (a bare unquoted string is rejected).
 */
function toOverride(key: string, value: unknown): string {
    const encoded = typeof value === "string" ? JSON.stringify(value) : String(value);
    return `-O${key}=${encoded}`;
}

/**
 * Whether a config key is one tuwunel recognizes (a DEFAULT_CONFIG field). The
 * shared element-web fixtures push Synapse-shaped keys (user_consent,
 * listeners, SMTP) through withConfig before their own homeserver-type skip
 * runs; forwarding those as -O flags makes tuwunel reject its own startup, so
 * only known fields reach the command line and the rest are dropped.
 * @param key The config key to test.
 * @returns True if the key is a tuwunel config field.
 */
function isTuwunelConfigField(key: string): key is keyof TuwunelConfig {
    return Object.prototype.hasOwnProperty.call(DEFAULT_CONFIG, key);
}

/**
 * A started TuwunelContainer, exposing the Client-Server API and a small
 * Synapse-compatible admin surface for test registration.
 */
export class StartedTuwunelContainer extends AbstractStartedContainer implements StartedHomeserverContainer {
    public readonly csApi: ClientServerApi;

    public constructor(
        container: StartedTestContainer,
        public readonly baseUrl: string,
        private readonly registrationSharedSecret: string,
    ) {
        super(container);
        this.csApi = new ClientServerApi(this.baseUrl);
    }

    public setRequest(request: APIRequestContext): void {
        this.csApi.setRequest(request);
    }

    public async onTestFinished(_testInfo: TestInfo): Promise<void> {
        // XXX: per-spec cleanup (publicRooms hide, user purge) is M3 work
    }

    private requestContext(): APIRequestContext {
        // XXX: ClientServerApi doesn't expose its request context publicly
        const ctx = (this.csApi as unknown as {_request?: APIRequestContext})._request;
        if (!ctx) {
            throw new Error("No request context; call setRequest first");
        }
        return ctx;
    }

    /**
     * Registers a user via the Synapse-compatible admin register endpoint.
     * @param username The localpart of the user to register.
     * @param password The password to set.
     * @param displayName The optional display name to set.
     * @param admin Whether the user should be granted admin privileges.
     * @returns The credentials of the newly-registered user.
     * @throws Error If the nonce GET or register POST fails.
     */
    private async registerUserInternal(
        username: string,
        password: string,
        displayName: string | undefined,
        admin: boolean,
    ): Promise<Credentials> {
        const url = `${this.baseUrl}/_synapse/admin/v1/register`;
        const ctx = this.requestContext();

        // Fetch the nonce
        const nonceResp = await ctx.fetch(url, {method: "GET"});
        if (!nonceResp.ok()) {
            throw new Error(`Nonce GET failed: ${nonceResp.status()} ${await nonceResp.text()}`);
        }
        const {nonce} = (await nonceResp.json()) as {nonce: string};

        // Sign the request with the shared secret
        const mac = crypto
            .createHmac("sha1", this.registrationSharedSecret)
            .update(`${nonce}\0${username}\0${password}\0${admin ? "admin" : "notadmin"}`)
            .digest("hex");

        // POST the registration
        const resp = await ctx.fetch(url, {
            method: "POST",
            data: {nonce, username, password, admin, mac, displayname: displayName},
        });
        if (!resp.ok()) {
            throw new Error(`Register POST failed: ${resp.status()} ${await resp.text()}`);
        }
        const data = (await resp.json()) as {
            user_id: string;
            access_token: string;
            device_id: string;
            home_server?: string;
        };

        return {
            homeServer: data.home_server || data.user_id.split(":").slice(1).join(":"),
            homeserverBaseUrl: this.baseUrl,
            accessToken: data.access_token,
            userId: data.user_id,
            deviceId: data.device_id,
            password,
            displayName,
            username,
        };
    }

    /**
     * Registers a non-admin user.
     * @param username The localpart of the user to register.
     * @param password The password to set.
     * @param displayName The optional display name to set.
     * @returns The credentials of the newly-registered user.
     */
    public registerUser(username: string, password: string, displayName?: string): Promise<Credentials> {
        return this.registerUserInternal(username, password, displayName, false);
    }

    /**
     * Logs an existing user in.
     * @param userId The user ID to log in as.
     * @param password The user's password.
     * @returns The credentials of the logged-in user.
     */
    public async loginUser(userId: string, password: string): Promise<Credentials> {
        return {
            ...(await this.csApi.loginUser(userId, password)),
            homeserverBaseUrl: this.baseUrl,
        };
    }

    /**
     * Binds a 3pid to a user. Not implemented: tuwunel does not expose a
     * Synapse-style PUT /_synapse/admin/v2/users/... endpoint.
     * @throws Error Always; specs depending on this must be skipped.
     */
    public async setThreepid(_userId: string, _medium: string, _address: string): Promise<void> {
        throw new Error("setThreepid: not implemented; spec must be skipped"); // XXX: no admin v2 user endpoint
    }
}
