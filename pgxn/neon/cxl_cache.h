#ifndef NEON_CXL_CACHE_H
#define NEON_CXL_CACHE_H

#include "postgres.h"

typedef struct NeonCxlPageLocation
{
	unsigned char pool_uuid[16];
	uint64		pool_epoch;
	uint32		region_id;
	uint64		region_epoch;
	uint64		slot_id;
	uint64		expected_control;
	uint64		absolute_offset;
	uint32		length;
	uint32		checksum_crc32c;
} NeonCxlPageLocation;

extern char *neon_cxl_cache_file;
extern bool neon_cxl_cache_enabled;

extern bool neon_cxl_cache_read(const NeonCxlPageLocation *location,
								char page[BLCKSZ],
								char **failure_reason);
extern void neon_cxl_cache_init_gucs(void);

#endif
