-- Fixture migration for the maverick-testdb self-test.
CREATE TABLE widgets (
    id integer PRIMARY KEY,
    weight real NOT NULL
);
INSERT INTO widgets (id, weight) VALUES (1, 0.1);
