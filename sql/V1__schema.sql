CREATE TABLE hash(
    id SERIAL PRIMARY KEY,
    hash BYTEA NOT NULL UNIQUE CHECK (
        length(hash) = 64
    )
);

CREATE TABLE tag(
    id SERIAL PRIMARY KEY,
    name TEXT NOT NULL UNIQUE CHECK (
        length(name) BETWEEN 1 AND 255
    )
);

CREATE TABLE hash_tag(
    hash_id INTEGER NOT NULL REFERENCES hash(id),
    tag_id INTEGER NOT NULL REFERENCES tag(id),
    PRIMARY KEY (hash_id, tag_id)
);
