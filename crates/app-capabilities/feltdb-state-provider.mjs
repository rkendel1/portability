import { createFeltDB } from '@feltdb/core';

const [operation, storePath, namespace, key, value] = process.argv.slice(2);

if (!operation || !storePath || !namespace || key === undefined) {
  console.error('usage: feltdb-state-provider.mjs <read|write|delete|list> <store> <namespace> <key> [base64-value]');
  process.exit(64);
}

const db = createFeltDB({ namespace, path: storePath });
const state = db.collection(`app_state_${Buffer.from(namespace).toString('hex')}`);

try {
  if (operation === 'read') {
    const record = await state.get(key);
    if (!record) {
      throw new Error(`state key not found: ${key}`);
    }
    process.stdout.write(record.value);
  } else if (operation === 'write') {
    if (value === undefined) {
      throw new Error('write requires a base64 value');
    }
    const record = await state.get(key);
    if (record) {
      await state.update(key, { value });
    } else {
      await state.insert({ value }, key);
    }
  } else if (operation === 'delete') {
    await state.delete(key);
  } else if (operation === 'list') {
    const prefix = key === '' || key.endsWith('/') ? key : `${key}/`;
    const names = new Set();
    for (const record of await state.all()) {
      if (!record.id.startsWith(prefix)) {
        continue;
      }
      const remainder = record.id.slice(prefix.length);
      if (remainder !== '') {
        names.add(remainder.split('/')[0]);
      }
    }
    process.stdout.write(JSON.stringify([...names].sort()));
  } else {
    throw new Error(`unsupported operation: ${operation}`);
  }
} finally {
  if (typeof state.close === 'function') {
    state.close();
  }
  if (typeof db.close === 'function') {
    db.close();
  }
}
