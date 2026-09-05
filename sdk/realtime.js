const DEFAULT_URL = "wss://gap.geta.team/v1/realtime";

export class GapRealtime {
  constructor({ token, tokenProvider, url = DEFAULT_URL, reconnect = true } = {}) {
    if (!token && typeof tokenProvider !== "function") {
      throw new TypeError("token or tokenProvider is required");
    }
    this.token = token;
    this.tokenProvider = tokenProvider;
    this.url = url;
    this.reconnect = reconnect;
    this.socket = null;
    this.channels = new Map();
    this.listeners = new Map();
    this.retry = 0;
    this.closed = false;
  }

  on(type, listener) {
    const listeners = this.listeners.get(type) || new Set();
    listeners.add(listener);
    this.listeners.set(type, listeners);
    return () => listeners.delete(listener);
  }

  emit(type, value) {
    for (const listener of this.listeners.get(type) || []) listener(value);
  }

  async connect() {
    this.closed = false;
    const token = this.tokenProvider ? await this.tokenProvider() : this.token;
    if (typeof token !== "string" || !token) throw new Error("token provider returned no token");
    const socket = new WebSocket(this.url);
    this.socket = socket;
    await new Promise((resolve, reject) => {
      socket.addEventListener("open", resolve, { once: true });
      socket.addEventListener("error", () => reject(new Error("realtime connection failed")), { once: true });
    });
    socket.addEventListener("message", event => this.handle(JSON.parse(event.data)));
    socket.addEventListener("close", event => this.handleClose(event));
    socket.send(JSON.stringify({ action: "authenticate", token }));
    return new Promise((resolve, reject) => {
      const offReady = this.on("authenticated", value => {
        offReady(); offError(); resolve(value);
      });
      const offError = this.on("error", error => {
        offReady(); offError(); reject(new Error(error.error));
      });
    });
  }

  handle(message) {
    if (message.type === "authenticated") {
      this.retry = 0;
      for (const [channel, after] of this.channels) this.send("subscribe", { channel, after });
    }
    this.emit(message.type, message);
    if (message.type === "message") this.emit(`message:${message.channel}`, message);
  }

  handleClose(event) {
    this.emit("close", event);
    if (this.closed || !this.reconnect) return;
    const delay = Math.min(30_000, 500 * (2 ** this.retry++));
    setTimeout(() => this.connect().catch(error => this.emit("connection_error", error)), delay);
  }

  send(action, fields = {}) {
    if (!this.socket || this.socket.readyState !== WebSocket.OPEN) throw new Error("realtime is not connected");
    this.socket.send(JSON.stringify({ action, ...fields }));
  }

  subscribe(channel, listener, { after = 0 } = {}) {
    this.channels.set(channel, after);
    const off = this.on(`message:${channel}`, listener);
    if (this.socket?.readyState === WebSocket.OPEN) this.send("subscribe", { channel, after });
    return () => { off(); this.unsubscribe(channel); };
  }

  unsubscribe(channel) {
    this.channels.delete(channel);
    if (this.socket?.readyState === WebSocket.OPEN) this.send("unsubscribe", { channel });
  }

  publish(channel, payload, { persist = false } = {}) {
    this.send("publish", { channel, payload, persist });
  }

  close() {
    this.closed = true;
    this.socket?.close(1000, "client closed");
  }
}

export default GapRealtime;
