/*
 * Glue between leviculum-esp and Bosch's BME690 SensorAPI (bme69x).
 *
 * `struct bme69x_dev` is deliberately opaque to the Rust side — mirroring
 * it field-for-field would mean copying Bosch's calibration structs into
 * Rust just to reach the three function pointers at the top. The C side
 * fills them instead: the Rust side allocates a byte buffer sized by
 * bme69x_glue_dev_size() and hands it to bme69x_glue_bind(), which sets
 * the interface fields in the struct's own ABI.
 *
 * This file is ours; the SensorAPI itself stays outside the crate.
 */
#include <stddef.h>
#include <string.h>

#include "bme69x.h"
#include "bme69x_defs.h"

size_t bme69x_glue_dev_size(void) { return sizeof(struct bme69x_dev); }
size_t bme69x_glue_dev_align(void) { return _Alignof(struct bme69x_dev); }

/*
 * Populate the fields bme69x_init() reads: the I2C interface selector,
 * the user pointer handed back to every callback, and the three
 * callbacks themselves.
 */
void bme69x_glue_bind(struct bme69x_dev *dev, void *intf_ptr,
                      bme69x_read_fptr_t rd, bme69x_write_fptr_t wr,
                      bme69x_delay_us_fptr_t del)
{
    memset(dev, 0, sizeof(*dev));
    dev->intf_ptr = intf_ptr;
    dev->intf = BME69X_I2C_INTF;
    dev->read = rd;
    dev->write = wr;
    dev->delay_us = del;
}

uint8_t bme69x_glue_chip_id(const struct bme69x_dev *dev)
{
    return dev->chip_id;
}
