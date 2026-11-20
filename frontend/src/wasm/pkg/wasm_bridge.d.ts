/* tslint:disable */
/* eslint-disable */
/**
 * The `ReadableStreamType` enum.
 *
 * *This API requires the following crate features to be activated: `ReadableStreamType`*
 */

export type ReadableStreamType = "bytes";

export class IntoUnderlyingByteSource {
    private constructor();
    free(): void;
    [Symbol.dispose](): void;
    cancel(): void;
    pull(controller: ReadableByteStreamController): Promise<any>;
    start(controller: ReadableByteStreamController): void;
    readonly autoAllocateChunkSize: number;
    readonly type: ReadableStreamType;
}

export class IntoUnderlyingSink {
    private constructor();
    free(): void;
    [Symbol.dispose](): void;
    abort(reason: any): Promise<any>;
    close(): Promise<any>;
    write(chunk: any): Promise<any>;
}

export class IntoUnderlyingSource {
    private constructor();
    free(): void;
    [Symbol.dispose](): void;
    cancel(): void;
    pull(controller: ReadableStreamDefaultController): Promise<any>;
}

export class WasmReceiveFileResult {
    private constructor();
    free(): void;
    [Symbol.dispose](): void;
    readonly bytes: Uint8Array;
    readonly bytesArray: Array<any>;
    readonly file_name: string;
    readonly file_names: string[];
}

export class WasmSendResult {
    private constructor();
    free(): void;
    [Symbol.dispose](): void;
    readonly hash: string;
    readonly size: bigint;
    readonly ticket: string;
}

/**
 * Fetch sender metadata JSON for a ticket (no file download).
 */
export function fetch_ticket_metadata(ticket: string, relay_json?: string | null): Promise<string>;

/**
 * Check which relay the app can reach. Returns JSON (`RelayStatusResponse`).
 */
export function get_relay_status(relay_json?: string | null): Promise<string>;

export function init(): void;

/**
 * Download a ticket into memory (single file or folder collection).
 */
export function receive_file(ticket: string, relay_json?: string | null): Promise<WasmReceiveFileResult>;

/**
 * Share a single in-memory file (relay-only ticket).
 */
export function send_file(file_name: string, bytes: Uint8Array, metadata_json?: string | null, relay_json?: string | null): Promise<WasmSendResult>;

/**
 * Share one or more in-memory files or folders (relay-only ticket).
 */
export function send_items(names: Array<any>, bytes_array: Array<any>, entry_type: string, metadata_json?: string | null, relay_json?: string | null): Promise<WasmSendResult>;

/**
 * Register `(eventName, payload?) => void` for transfer progress events.
 */
export function set_event_callback(callback: Function): void;

/**
 * Persist node identity across page reloads (hex-encoded iroh secret key).
 */
export function set_secret_key(secret_hex: string): void;

/**
 * Bind a relay-only iroh endpoint and return its node id (smoke test).
 */
export function smoke_test_endpoint(): Promise<string>;

/**
 * Stop the active browser share session, if any.
 */
export function stop_sharing(): void;

/**
 * Verify connectivity to configured relay servers. Returns JSON (`VerifyRelaysResponse`).
 */
export function verify_relays(relay_json: string): Promise<string>;

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly __wbg_wasmreceivefileresult_free: (a: number, b: number) => void;
    readonly __wbg_wasmsendresult_free: (a: number, b: number) => void;
    readonly fetch_ticket_metadata: (a: number, b: number, c: number, d: number) => any;
    readonly get_relay_status: (a: number, b: number) => any;
    readonly init: () => void;
    readonly receive_file: (a: number, b: number, c: number, d: number) => any;
    readonly send_file: (a: number, b: number, c: number, d: number, e: number, f: number, g: number, h: number) => any;
    readonly send_items: (a: any, b: any, c: number, d: number, e: number, f: number, g: number, h: number) => any;
    readonly set_event_callback: (a: any) => void;
    readonly set_secret_key: (a: number, b: number) => [number, number];
    readonly smoke_test_endpoint: () => any;
    readonly stop_sharing: () => void;
    readonly verify_relays: (a: number, b: number) => any;
    readonly wasmreceivefileresult_bytes: (a: number) => [number, number];
    readonly wasmreceivefileresult_bytesArray: (a: number) => any;
    readonly wasmreceivefileresult_file_name: (a: number) => [number, number];
    readonly wasmreceivefileresult_file_names: (a: number) => [number, number];
    readonly wasmsendresult_hash: (a: number) => [number, number];
    readonly wasmsendresult_size: (a: number) => bigint;
    readonly wasmsendresult_ticket: (a: number) => [number, number];
    readonly __wbg_intounderlyingsink_free: (a: number, b: number) => void;
    readonly intounderlyingsink_abort: (a: number, b: any) => any;
    readonly intounderlyingsink_close: (a: number) => any;
    readonly intounderlyingsink_write: (a: number, b: any) => any;
    readonly __wbg_intounderlyingbytesource_free: (a: number, b: number) => void;
    readonly __wbg_intounderlyingsource_free: (a: number, b: number) => void;
    readonly intounderlyingbytesource_autoAllocateChunkSize: (a: number) => number;
    readonly intounderlyingbytesource_cancel: (a: number) => void;
    readonly intounderlyingbytesource_pull: (a: number, b: any) => any;
    readonly intounderlyingbytesource_start: (a: number, b: any) => void;
    readonly intounderlyingbytesource_type: (a: number) => number;
    readonly intounderlyingsource_cancel: (a: number) => void;
    readonly intounderlyingsource_pull: (a: number, b: any) => any;
    readonly ring_core_0_17_14__bn_mul_mont: (a: number, b: number, c: number, d: number, e: number, f: number) => void;
    readonly wasm_bindgen__convert__closures_____invoke__he82df4a0d96562b5: (a: number, b: number, c: any) => [number, number];
    readonly wasm_bindgen__convert__closures_____invoke__h036b99db8a65afe6: (a: number, b: number, c: any, d: any) => void;
    readonly wasm_bindgen__convert__closures_____invoke__h9367082d8eccbd00: (a: number, b: number, c: any) => void;
    readonly wasm_bindgen__convert__closures_____invoke__h807f3f1ab8fb4867: (a: number, b: number, c: any) => void;
    readonly wasm_bindgen__convert__closures_____invoke__h8250dfb3b90a7b01: (a: number, b: number, c: any) => void;
    readonly wasm_bindgen__convert__closures_____invoke__h7a58a1fb97e9b31e: (a: number, b: number) => void;
    readonly wasm_bindgen__convert__closures_____invoke__h6422a0672ed31c1a: (a: number, b: number) => void;
    readonly wasm_bindgen__convert__closures_____invoke__h951fc8d800a8f655: (a: number, b: number) => void;
    readonly wasm_bindgen__convert__closures_____invoke__h2acc9c0d7cfe084f: (a: number, b: number) => void;
    readonly __wbindgen_malloc: (a: number, b: number) => number;
    readonly __wbindgen_realloc: (a: number, b: number, c: number, d: number) => number;
    readonly __wbindgen_exn_store: (a: number) => void;
    readonly __externref_table_alloc: () => number;
    readonly __wbindgen_externrefs: WebAssembly.Table;
    readonly __wbindgen_free: (a: number, b: number, c: number) => void;
    readonly __wbindgen_destroy_closure: (a: number, b: number) => void;
    readonly __externref_table_dealloc: (a: number) => void;
    readonly __externref_drop_slice: (a: number, b: number) => void;
    readonly __wbindgen_start: () => void;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;

/**
 * Instantiates the given `module`, which can either be bytes or
 * a precompiled `WebAssembly.Module`.
 *
 * @param {{ module: SyncInitInput }} module - Passing `SyncInitInput` directly is deprecated.
 *
 * @returns {InitOutput}
 */
export function initSync(module: { module: SyncInitInput } | SyncInitInput): InitOutput;

/**
 * If `module_or_path` is {RequestInfo} or {URL}, makes a request and
 * for everything else, calls `WebAssembly.instantiate` directly.
 *
 * @param {{ module_or_path: InitInput | Promise<InitInput> }} module_or_path - Passing `InitInput` directly is deprecated.
 *
 * @returns {Promise<InitOutput>}
 */
export default function __wbg_init (module_or_path?: { module_or_path: InitInput | Promise<InitInput> } | InitInput | Promise<InitInput>): Promise<InitOutput>;
