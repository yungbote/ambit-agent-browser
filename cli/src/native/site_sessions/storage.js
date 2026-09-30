// Host-only synthetic origin document: no remote script executes here.
async function custodyStorage(mode, input) {
  const binaryKinds = new Set([
    'ArrayBuffer', 'DataView', 'Int8Array', 'Uint8Array', 'Uint8ClampedArray',
    'Int16Array', 'Uint16Array', 'Int32Array', 'Uint32Array', 'Float16Array',
    'Float32Array', 'Float64Array', 'BigInt64Array', 'BigUint64Array',
  ]);
  const bytes64 = bytes => {
    let text = '';
    for (const byte of bytes) text += String.fromCharCode(byte);
    return btoa(text);
  };
  const decodeBytes = text => Uint8Array.from(atob(text), character => character.charCodeAt(0));
  async function encode(value, seen = new WeakSet(), depth = 0) {
    if (depth > 64) throw new Error('Unencodable record');
    if (value === null || typeof value === 'string' || typeof value === 'boolean') return value;
    if (typeof value === 'undefined') return { $: 'undefined' };
    if (typeof value === 'number') return Number.isFinite(value) && !Object.is(value, -0)
      ? value : { $: 'number', v: Object.is(value, -0) ? '-0' : String(value) };
    if (typeof value === 'bigint') return { $: 'bigint', v: String(value) };
    if (typeof value !== 'object' || seen.has(value)) throw new Error('Unencodable record');
    seen.add(value);
    const next = child => encode(child, seen, depth + 1);
    if (Array.isArray(value)) return Promise.all(value.map(next));
    if (value instanceof Date) return { $: 'date', v: Number.isNaN(value.getTime()) ? null : value.getTime() };
    if (value instanceof RegExp) return { $: 'regexp', source: value.source, flags: value.flags };
    if (value instanceof File) return { $: 'file', type: value.type, name: value.name,
      lastModified: value.lastModified, base64: bytes64(new Uint8Array(await value.arrayBuffer())) };
    if (value instanceof Blob) return { $: 'blob', type: value.type,
      base64: bytes64(new Uint8Array(await value.arrayBuffer())) };
    if (value instanceof ArrayBuffer || ArrayBuffer.isView(value)) {
      const kind = value.constructor.name;
      if (!binaryKinds.has(kind)) throw new Error('Unencodable record');
      const bytes = value instanceof ArrayBuffer ? new Uint8Array(value)
        : new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
      return { $: 'binary', kind, base64: bytes64(bytes) };
    }
    if (value instanceof Map) return { $: 'map', entries: await Promise.all(
      [...value].map(async ([key, entry]) => [await next(key), await next(entry)])) };
    if (value instanceof Set) return { $: 'set', values: await Promise.all([...value].map(next)) };
    if (value instanceof Error) return { $: 'error', name: value.name, message: value.message };
    if (Object.getPrototypeOf(value) !== Object.prototype) throw new Error('Unencodable record');
    return { $: 'object', entries: await Promise.all(Object.entries(value)
      .map(async ([key, entry]) => [key, await next(entry)])) };
  }
  function decode(value) {
    if (value === null || typeof value !== 'object') return value;
    if (Array.isArray(value)) return value.map(decode);
    switch (value.$) {
      case 'undefined': return undefined;
      case 'number': return value.v === '-0' ? -0 : Number(value.v);
      case 'bigint': return BigInt(value.v);
      case 'date': return new Date(value.v === null ? NaN : value.v);
      case 'regexp': return new RegExp(value.source, value.flags);
      case 'binary': {
        const bytes = decodeBytes(value.base64);
        if (value.kind === 'ArrayBuffer') return bytes.buffer;
        if (value.kind === 'DataView') return new DataView(bytes.buffer);
        if (typeof globalThis[value.kind] !== 'function') throw new Error('Unsupported binary type');
        return new globalThis[value.kind](bytes.buffer);
      }
      case 'blob': return new Blob([decodeBytes(value.base64)], { type: value.type });
      case 'file': return new File([decodeBytes(value.base64)], value.name,
        { type: value.type, lastModified: value.lastModified });
      case 'map': return new Map(value.entries.map(([key, entry]) => [decode(key), decode(entry)]));
      case 'set': return new Set(value.values.map(decode));
      case 'object': return Object.fromEntries(value.entries.map(([key, entry]) => [key, decode(entry)]));
      case 'error': { const error = new Error(value.message); error.name = value.name; return error; }
      default: throw new Error('Unsupported record tag');
    }
  }
  const request = value => new Promise((resolve, reject) => {
    value.onsuccess = () => resolve(value.result);
    value.onerror = () => reject(new Error('Storage request failed'));
    value.onblocked = () => reject(new Error('Storage request blocked'));
  });
  const transaction = value => new Promise((resolve, reject) => {
    value.oncomplete = () => resolve();
    value.onerror = value.onabort = () => reject(new Error('Storage transaction failed'));
  });
  const highwaterValue = (path, key) => {
    const value = {};
    if (path === null) return value;
    let target = value;
    const parts = path.split('.');
    for (let index = 0; index < parts.length; index++) {
      const next = index === parts.length - 1 ? key : {};
      Object.defineProperty(target, parts[index], { value: next, enumerable: true, writable: true, configurable: true });
      target = next;
    }
    return value;
  };
  const nextKey = (database, name) => new Promise((resolve, reject) => {
    const writing = database.transaction(name, 'readwrite');
    const store = writing.objectStore(name);
    let result;
    let failure;
    const generated = store.add({});
    generated.onsuccess = () => { result = generated.result; writing.abort(); };
    generated.onerror = event => {
      event.preventDefault();
      event.stopPropagation();
      if (generated.error?.name === 'ConstraintError') result = 'exhausted';
      else failure = new Error('Storage generator capture failed');
      writing.abort();
    };
    writing.onabort = () => failure ? reject(failure) : resolve(result);
    writing.onerror = () => reject(new Error('Storage generator capture failed'));
  });
  if (mode === 'import') {
    // Decode before changing Chrome state; invalid structured data is atomic.
    const databases = input.indexedDB.map(database => ({ ...database,
      stores: database.stores.map(store => ({ ...store,
        records: store.records.map(record => ({
          ...(Object.hasOwn(record, 'key') ? { key: decode(record.key) } : {}),
          value: decode(record.value),
        })),
      })),
    }));
    localStorage.clear();
    for (const [key, value] of input.localStorage) localStorage.setItem(key, value);
    for (const database of databases) {
      await request(indexedDB.deleteDatabase(database.name));
      const opening = indexedDB.open(database.name, database.version);
      opening.onupgradeneeded = () => {
        for (const store of database.stores) {
          const created = opening.result.createObjectStore(store.name,
            { keyPath: store.keyPath, autoIncrement: store.autoIncrement });
          for (const index of store.indexes) created.createIndex(index.name, index.keyPath,
            { unique: index.unique, multiEntry: index.multiEntry });
        }
      };
      const opened = await request(opening);
      try {
        if (!database.stores.length) continue;
        const writing = opened.transaction(database.stores.map(store => store.name), 'readwrite');
        const finished = transaction(writing);
        for (const store of database.stores) {
          const target = writing.objectStore(store.name);
          if (store.autoIncrement && Object.hasOwn(store, 'nextKey')) {
            const previous = store.nextKey === 'exhausted' ? 2 ** 53 : store.nextKey - 1;
            // Set the generator on an empty store before restoring real rows;
            // otherwise deleting the probe could delete a legitimate row.
            const probe = highwaterValue(store.keyPath, previous);
            store.keyPath === null ? target.put(probe, previous) : target.put(probe);
            target.delete(previous);
          }
          for (const record of store.records) Object.hasOwn(record, 'key')
            ? target.put(record.value, record.key) : target.put(record.value);
        }
        await finished;
      } finally { opened.close(); }
    }
    return { imported: true };
  }
  // One cursor value is held while its encoding drains. IndexedDB commits
  // a transaction with no pending requests; a cheap count request keeps the
  // same readonly snapshot alive across Blob encoding and host backpressure.
  // Closing the synthetic target also aborts this readonly transaction.
  async function* rows(database, name) {
    const reading = database.transaction(name, 'readonly');
    const store = reading.objectStore(name);
    const cursor = store.openCursor();
    let current;
    let ready = false;
    let advance = false;
    let stopped = false;
    let failure;
    let wake;
    const failed = () => { failure = new Error('Storage cursor failed'); wake?.(); };
    cursor.onerror = failed;
    cursor.onsuccess = () => {
      current = cursor.result;
      ready = true;
      if (!current) stopped = true;
      wake?.();
    };
    reading.onerror = failed;
    reading.onabort = () => { if (!stopped) failed(); };
    const keepReading = () => {
      if (stopped || failure) return;
      const pending = store.count(IDBKeyRange.only(0));
      pending.onerror = failed;
      pending.onsuccess = () => {
        if (stopped || failure) return;
        if (advance) { advance = false; current.continue(); }
        keepReading();
      };
    };
    keepReading();
    try {
      while (true) {
        if (!ready && !failure) await new Promise(resolve => { wake = resolve; });
        wake = undefined;
        if (failure) throw failure;
        if (!current) return;
        yield { key: current.key, value: current.value };
        ready = false;
        advance = true;
      }
    } finally {
      stopped = true;
      try { reading.abort(); } catch { /* Already finished. */ }
    }
  }
  async function* exporting() {
    const omitted = [];
    yield '{"localStorage":[';
    for (let index = 0; index < localStorage.length; index++) {
      const key = localStorage.key(index);
      yield (index ? ',' : '') + JSON.stringify([key, localStorage.getItem(key)]);
    }
    yield '],"indexedDB":[';
    let databaseIndex = 0;
    for (const info of await indexedDB.databases()) {
      const opened = await request(indexedDB.open(info.name));
      try {
        yield (databaseIndex++ ? ',' : '') + '{"name":' + JSON.stringify(opened.name)
          + ',"version":' + opened.version + ',"stores":[';
        let storeIndex = 0;
        for (const name of opened.objectStoreNames) {
          const store = opened.transaction(name, 'readonly').objectStore(name);
          const indexes = [...store.indexNames].map(name => {
            const index = store.index(name);
            return { name, keyPath: index.keyPath, unique: index.unique, multiEntry: index.multiEntry };
          });
          yield (storeIndex++ ? ',' : '') + '{"name":' + JSON.stringify(name)
            + ',"keyPath":' + JSON.stringify(store.keyPath) + ',"autoIncrement":' + store.autoIncrement
            + ',"indexes":' + JSON.stringify(indexes) + ',"records":[';
          let recordIndex = 0;
          for await (const record of rows(opened, name)) {
            let encoded;
            try {
              encoded = { ...(store.keyPath === null ? { key: await encode(record.key) } : {}),
                value: await encode(record.value) };
            } catch { omitted.push({ what: 'indexed_db', bytes: 0 }); continue; }
            yield (recordIndex++ ? ',' : '') + JSON.stringify(encoded);
          }
          yield '],"nextKey":' + JSON.stringify(store.autoIncrement ? await nextKey(opened, name) : null) + '}';
        }
        yield ']}';
      } finally { opened.close(); }
    }
    yield '],"omitted":' + JSON.stringify(omitted) + '}';
  }
  async function* chunks() {
    for await (const text of exporting()) {
      for (let start = 0; start < text.length;) {
        let end = Math.min(start + input.chunkCharacters, text.length);
        // A JSON document may contain literal non-BMP characters. Keep
        // each CDP string valid rather than emitting a lone surrogate.
        if (end < text.length && text.charCodeAt(end - 1) >= 0xd800 && text.charCodeAt(end - 1) <= 0xdbff) end++;
        yield text.slice(start, end);
        start = end;
      }
    }
  }
  return chunks();
}
