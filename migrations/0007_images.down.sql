-- Reverts 0007_images. Without the table, image garbage collection has nothing to prune.
DROP TABLE images;
